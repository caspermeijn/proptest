//-
// Copyright 2017, 2018 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use core::{fmt, iter};
#[cfg(feature = "std")]
use std::panic::{self, AssertUnwindSafe};
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::SeqCst;
use std_facade::{Box, Arc, Vec, BTreeMap, String};

#[cfg(feature = "fork")]
use std::fs;
#[cfg(feature = "fork")]
use std::env;
#[cfg(feature = "fork")]
use std::cell::RefCell;
#[cfg(feature = "fork")]
use rusty_fork;
#[cfg(feature = "fork")]
use tempfile;

use test_runner::{TestRng, Seed};
use test_runner::errors::*;
use test_runner::config::*;
use test_runner::reason::*;
use test_runner::result_cache::*;
#[cfg(feature = "fork")]
use test_runner::replay;
use strategy::*;

#[cfg(feature = "fork")]
const ENV_FORK_FILE: &'static str = "_PROPTEST_FORKFILE";

type RejectionDetail = BTreeMap<Reason, u32>;

/// State used when running a proptest test.
#[derive(Clone)]
pub struct TestRunner {
    config: Config,
    successes: u32,
    local_rejects: u32,
    global_rejects: u32,
    rng: TestRng,
    flat_map_regens: Arc<AtomicUsize>,

    local_reject_detail: RejectionDetail,
    global_reject_detail: RejectionDetail,
}

impl fmt::Debug for TestRunner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TestRunner")
            .field("config", &self.config)
            .field("successes", &self.successes)
            .field("local_rejects", &self.local_rejects)
            .field("global_rejects", &self.global_rejects)
            .field("rng", &"<TestRng>")
            .field("flat_map_regens", &self.flat_map_regens)
            .field("local_reject_detail", &self.local_reject_detail)
            .field("global_reject_detail", &self.global_reject_detail)
            .finish()
    }
}

impl fmt::Display for TestRunner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "\tsuccesses: {}\n\
                   \tlocal rejects: {}\n",
               self.successes, self.local_rejects)?;
        for (whence, count) in &self.local_reject_detail {
            writeln!(f, "\t\t{} times at {}", count, whence)?;
        }
        writeln!(f, "\tglobal rejects: {}", self.global_rejects)?;
        for (whence, count) in &self.global_reject_detail {
            writeln!(f, "\t\t{} times at {}", count, whence)?;
        }

        Ok(())
    }
}

/// Equivalent to: `TestRunner::new(Config::default())`.
impl Default for TestRunner {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(feature = "fork")]
#[derive(Debug)]
struct ForkOutput {
    file: Option<fs::File>,
}

#[cfg(feature = "fork")]
impl ForkOutput {
    fn append(&mut self, result: &TestCaseResult) {
        if let Some(ref mut file) = self.file {
            replay::append(file, result)
                .expect("Failed to append to replay file");
        }
    }

    fn terminate(&mut self) {
        if let Some(ref mut file) = self.file {
            replay::terminate(file)
                .expect("Failed to append to replay file");
        }
    }

    fn empty() -> Self {
        ForkOutput { file: None }
    }

    fn is_in_fork(&self) -> bool {
        self.file.is_some()
    }
}

#[cfg(not(feature = "fork"))]
#[derive(Debug)]
struct ForkOutput;

#[cfg(not(feature = "fork"))]
impl ForkOutput {
    fn append(&mut self, _result: &TestCaseResult) { }
    fn terminate(&mut self) { }
    fn empty() -> Self { ForkOutput }
    fn is_in_fork(&self) -> bool { false }
}

#[cfg(not(feature = "std"))]
fn call_test<V, F, R>
    (case: V, test: &F, replay: &mut R, _timeout: u32,
     cache: &mut dyn ResultCache, _: &mut ForkOutput) -> TestCaseResult
where
    V: fmt::Debug,
    F: Fn(V) -> TestCaseResult,
    R: Iterator<Item = TestCaseResult>,
{
    if let Some(result) = replay.next() {
        return result;
    }

    let cache_key = cache.key(&ResultCacheKey::new(&case));
    if let Some(result) = cache.get(cache_key) {
        return result.clone();
    }

    let result = test(case);
    result_cache.put(cache_key, &result);
    result
}

#[cfg(feature = "std")]
fn call_test<V, F, R>
    (case: V, test: &F, replay: &mut R, timeout: u32,
     result_cache: &mut dyn ResultCache, fork_output: &mut ForkOutput)
    -> TestCaseResult
where
    V: fmt::Debug,
    F: Fn(V) -> TestCaseResult,
    R: Iterator<Item = TestCaseResult>,
{
    use std::time;

    if let Some(result) = replay.next() {
        return result;
    }

    let cache_key = result_cache.key(&ResultCacheKey::new(&case));
    if let Some(result) = result_cache.get(cache_key) {
        return result.clone();
    }

    let time_start = time::Instant::now();

    let mut result = unwrap_or!(
        panic::catch_unwind(AssertUnwindSafe(|| test(case))),
        what => Err(TestCaseError::Fail(
            what.downcast::<&'static str>().map(|s| (*s).into())
                .or_else(|what| what.downcast::<String>().map(|b| (*b).into()))
                .or_else(|what| what.downcast::<Box<str>>().map(|b| (*b).into()))
                .unwrap_or_else(|_| "<unknown panic value>".into()))));

    // If there is a timeout and we exceeded it, fail the test here so we get
    // consistent behaviour. (The parent process cannot precisely time the test
    // cases itself.)
    if timeout > 0 && result.is_ok() {
        let elapsed = time_start.elapsed();
        let elapsed_millis = elapsed.as_secs() as u32 * 1000 +
            elapsed.subsec_nanos() / 1_000_000;

        if elapsed_millis > timeout {
            result = Err(TestCaseError::fail(
                format!("Timeout of {} ms exceeded: test took {} ms",
                        timeout, elapsed_millis)));
        }
    }

    result_cache.put(cache_key, &result);
    fork_output.append(&result);

    result
}

type TestRunResult<S> = Result<(), TestError<<S as Strategy>::Value>>;

impl TestRunner {
    /// Create a fresh `TestRunner` with the given configuration.
    pub fn new(config: Config) -> Self {
        TestRunner {
            config: config,
            successes: 0,
            local_rejects: 0,
            global_rejects: 0,
            rng: TestRng::default_rng(),
            flat_map_regens: Arc::new(AtomicUsize::new(0)),
            local_reject_detail: BTreeMap::new(),
            global_reject_detail: BTreeMap::new(),
        }
    }

    /// Create a fresh `TestRunner` with the same config and global counters as
    /// this one, but with local state reset and an independent `Rng` (but
    /// deterministic).
    pub(crate) fn partial_clone(&mut self) -> Self {
        TestRunner {
            config: self.config.clone(),
            successes: 0,
            local_rejects: 0,
            global_rejects: 0,
            rng: self.new_rng(),
            flat_map_regens: Arc::clone(&self.flat_map_regens),
            local_reject_detail: BTreeMap::new(),
            global_reject_detail: BTreeMap::new(),
        }
    }

    /// Returns the RNG for this test run.
    pub fn rng(&mut self) -> &mut TestRng {
        &mut self.rng
    }

    /// Create a new, independent but deterministic RNG from the RNG in this
    /// runner.
    pub fn new_rng(&mut self) -> TestRng {
        self.rng.gen_rng()
    }

    /// Returns the configuration of this runner.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run test cases against `f`, choosing inputs via `strategy`.
    ///
    /// If any failure cases occur, try to find a minimal failure case and
    /// report that. If invoking `f` panics, the panic is turned into a
    /// `TestCaseError::Fail`.
    ///
    /// If failure persistence is enabled, all persisted failing cases are
    /// tested first. If a later non-persisted case fails, its seed is
    /// persisted before returning failure.
    ///
    /// Returns success or failure indicating why the test as a whole failed.
    pub fn run<S : Strategy>(&mut self, strategy: &S,
                             test: impl Fn (S::Value) -> TestCaseResult)
                             -> TestRunResult<S> {
        if self.config.fork() {
            self.run_in_fork(strategy, test)
        } else {
            self.run_in_process(strategy, test)
        }
    }

    #[cfg(not(feature = "fork"))]
    fn run_in_fork<S : Strategy>
        (&mut self, _: &S, _: impl Fn (S::Value) -> TestCaseResult) -> TestRunResult<S>
    {
        unreachable!()
    }

    #[cfg(feature = "fork")]
    fn run_in_fork<S : Strategy>(&mut self, strategy: &S,
                                 test: impl Fn (S::Value) -> TestCaseResult)
                                 -> TestRunResult<S>
    {
        let mut test = Some(test);

        let test_name = rusty_fork::fork_test::fix_module_path(
            self.config.test_name.expect(
                "Must supply test_name when forking enabled"));
        let forkfile: RefCell<Option<tempfile::NamedTempFile>> =
            RefCell::new(None);
        let seed = self.rng.new_rng_seed();
        let mut replay = replay::Replay { seed, steps: vec![] };
        let mut child_count = 0;
        let timeout = self.config.timeout();

        loop {
            let (child_error, last_fork_file_len) = rusty_fork::fork(
                test_name,
                rusty_fork_id!(),
                |cmd| {
                    let mut forkfile = forkfile.borrow_mut();
                    if forkfile.is_none() {
                        *forkfile =
                            Some(tempfile::NamedTempFile::new().expect(
                                "Failed to create temporary file for fork"));
                        replay.init_file(
                            forkfile.as_mut().unwrap()).expect(
                            "Failed to initialise temporary file for fork");
                    }

                    cmd.env(ENV_FORK_FILE, forkfile.as_ref().unwrap().path());
                },
                |child, _| await_child(
                    child, &mut forkfile.borrow_mut().as_mut().unwrap(),
                    timeout),
                || match self.run_in_process(strategy, test.take().unwrap()) {
                    Ok(_) => (),
                    Err(e) =>
                        panic!("Test failed normally in child process.\n{}\n{}",
                               e, self),
                })
                .expect("Fork failed");

            let parsed = replay::Replay::parse_from(
                &mut forkfile.borrow_mut().as_mut().unwrap())
                .expect("Failed to re-read fork file");
            match parsed {
                replay::ReplayFileStatus::InProgress(new_replay) =>
                    replay = new_replay,
                replay::ReplayFileStatus::Terminated(new_replay) => {
                    replay = new_replay;
                    break;
                },
                replay::ReplayFileStatus::Corrupt =>
                    panic!("Child process corrupted replay file"),
            }

            // The child only terminates early if it outright crashes or we
            // kill it due to timeout, so add a synthetic failure to the
            // output. But only do this if the length of the fork file is the
            // same as when we last saw it, or if the child was not killed due
            // to timeout. (This is because the child could have appended
            // something to the file after we gave up waiting for it but before
            // we were able to kill it).
            if last_fork_file_len.map_or(true, |last_fork_file_len| {
                last_fork_file_len == forkfile.borrow().as_ref().unwrap()
                    .as_file().metadata().map(|md| md.len()).unwrap_or(0)
            }) {
                let error = Err(child_error.unwrap_or(
                    TestCaseError::fail("Child process was terminated abruptly \
                                         but with successful status")));
                replay::append(forkfile.borrow_mut().as_mut().unwrap(), &error)
                    .expect("Failed to append to replay file");
                replay.steps.push(error);
            }

            // Bail if we've gone through too many processes in case the
            // shrinking process itself is crashing.
            child_count += 1;
            if child_count >= 10000 {
                return Err(TestError::Abort(
                    "Giving up after 10000 child processes crashed".into()));
            }
        }

        // Run through the steps in-process (without ever running the actual
        // tests) to produce the shrunken value and update the persistence
        // file.
        self.rng.set_seed(replay.seed);
        self.run_in_process_with_replay(
            strategy, |_| panic!("Ran past the end of the replay"),
            replay.steps.into_iter(), ForkOutput::empty())
    }

    fn run_in_process<S : Strategy>
        (&mut self, strategy: &S, test: impl Fn (S::Value) -> TestCaseResult)
         -> TestRunResult<S>
    {
        let (replay_steps, fork_output) = init_replay(&mut self.rng);
        self.run_in_process_with_replay(
            strategy, test, replay_steps.into_iter(), fork_output)
    }

    fn run_in_process_with_replay<S : Strategy>
        (&mut self, strategy: &S,
         test: impl Fn (S::Value) -> TestCaseResult,
         mut replay: impl Iterator<Item = TestCaseResult>,
         mut fork_output: ForkOutput)
        -> TestRunResult<S>
    {
        let old_rng = self.rng.clone();

        let persisted_failure_seeds: Vec<Seed> =
            self.config.failure_persistence
                .as_ref()
                .map(|f| f.load_persisted_failures(self.config.source_file))
                .unwrap_or_default();

        let mut result_cache = self.new_cache();

        for persisted_seed in persisted_failure_seeds {
            self.rng.set_seed(persisted_seed);
            self.gen_and_run_case(strategy, &test, &mut replay,
                                  &mut *result_cache, &mut fork_output)?;
        }
        self.rng = old_rng;

        while self.successes < self.config.cases {
            // Generate a new seed and make an RNG from that so that we know
            // what seed to persist if this case fails.
            let seed = self.rng.gen_get_seed();
            let result = self.gen_and_run_case(
                strategy, &test, &mut replay, &mut *result_cache, &mut fork_output);
            if let Err(TestError::Fail(_, ref value)) = result {
                if let Some(ref mut failure_persistence) = self.config.failure_persistence {
                    let source_file = &self.config.source_file;

                    // Don't update the persistence file if we're a child
                    // process. The parent relies on it remaining consistent
                    // and will take care of updating it itself.
                    if !fork_output.is_in_fork() {
                        failure_persistence.save_persisted_failure(
                            *source_file, seed, value);
                    }
                }
            }

            if let Err(e) = result {
                fork_output.terminate();
                return Err(e.into());
            }
        }

        fork_output.terminate();
        Ok(())
    }

    fn gen_and_run_case<S : Strategy>
        (&mut self, strategy: &S,
         f: &impl Fn (S::Value) -> TestCaseResult,
         replay: &mut impl Iterator<Item = TestCaseResult>,
         result_cache: &mut dyn ResultCache,
         fork_output: &mut ForkOutput)
        -> TestRunResult<S>
    {
        let case =
            unwrap_or!(strategy.new_tree(self), msg =>
                return Err(TestError::Abort(msg)));

        if self.run_one_with_replay(case, f, replay, result_cache, fork_output)? {
            self.successes += 1;
        }
        Ok(())
    }

    /// Run one specific test case against this runner.
    ///
    /// If the test fails, finds the minimal failing test case. If the test
    /// does not fail, returns whether it succeeded or was filtered out.
    ///
    /// This does not honour the `fork` config, and will not be able to
    /// terminate the run if it runs for longer than `timeout`. However, if the
    /// test function returns but took longer than `timeout`, the test case
    /// will fail.
    pub fn run_one<V : ValueTree>
        (&mut self, case: V,
         test: impl Fn (V::Value) -> TestCaseResult)
        -> Result<bool, TestError<V::Value>>
    {
        let mut result_cache = self.new_cache();
        self.run_one_with_replay(
            case, test,
            &mut iter::empty::<TestCaseResult>().fuse(),
            &mut *result_cache,
            &mut ForkOutput::empty())
    }

    fn run_one_with_replay<V : ValueTree>
        (&mut self, mut case: V,
         test: impl Fn (V::Value) -> TestCaseResult,
         replay: &mut impl Iterator<Item = TestCaseResult>,
         result_cache: &mut dyn ResultCache,
         fork_output: &mut ForkOutput)
        -> Result<bool, TestError<V::Value>>
    {
        let result = call_test(
            case.current(), &test,
            replay, self.config.timeout(),
            result_cache, fork_output);

        match result {
            Ok(_) => Ok(true),
            Err(TestCaseError::Fail(why)) => {
                let why = self.shrink(&mut case, test, replay,
                                      result_cache, fork_output)
                    .unwrap_or(why);
                Err(TestError::Fail(why, case.current()))
            },
            Err(TestCaseError::Reject(whence)) => {
                self.reject_global(whence)?;
                Ok(false)
            },
        }
    }

    fn shrink<V : ValueTree>
        (&mut self, case: &mut V,
         test: impl Fn (V::Value) -> TestCaseResult,
         replay: &mut impl Iterator<Item = TestCaseResult>,
         result_cache: &mut dyn ResultCache,
         fork_output: &mut ForkOutput)
        -> Option<Reason>
    {
        let mut last_failure = None;

        if case.simplify() {
            loop {
                let result = call_test(
                    case.current(), &test,
                    replay, self.config.timeout(),
                    result_cache, fork_output);

                match result {
                    // Rejections are effectively a pass here,
                    // since they indicate that any behaviour of
                    // the function under test is acceptable.
                    Ok(_) | Err(TestCaseError::Reject(..)) => {
                        if !case.complicate() {
                            break;
                        }
                    },
                    Err(TestCaseError::Fail(why)) => {
                        last_failure = Some(why);
                        if !case.simplify() {
                            break;
                        }
                    },
                }
            }
        }

        last_failure
    }

    /// Update the state to account for a local rejection from `whence`, and
    /// return `Ok` if the caller should keep going or `Err` to abort.
    pub fn reject_local(&mut self, whence: impl Into<Reason>)
                        -> Result<(), Reason> {
        if self.local_rejects >= self.config.max_local_rejects {
            Err("Too many local rejects".into())
        } else {
            self.local_rejects += 1;
            Self::insert_or_increment(&mut self.local_reject_detail,
                whence.into());
            Ok(())
        }
    }

    /// Update the state to account for a global rejection from `whence`, and
    /// return `Ok` if the caller should keep going or `Err` to abort.
    fn reject_global<T>(&mut self, whence: Reason) -> Result<(),TestError<T>> {
        if self.global_rejects >= self.config.max_global_rejects {
            Err(TestError::Abort("Too many global rejects".into()))
        } else {
            self.global_rejects += 1;
            Self::insert_or_increment(&mut self.global_reject_detail, whence);
            Ok(())
        }
    }

    /// Insert 1 or increment the rejection detail at key for whence.
    fn insert_or_increment(into: &mut RejectionDetail, whence: Reason) {
        into.entry(whence).and_modify(|count| { *count += 1 }).or_insert(1);
    }

    /// Increment the counter of flat map regenerations and return whether it
    /// is still under the configured limit.
    pub fn flat_map_regen(&self) -> bool {
        self.flat_map_regens.fetch_add(1, SeqCst) <
            self.config.max_flat_map_regens as usize
    }

    fn new_cache(&self) -> Box<dyn ResultCache> {
        (self.config.result_cache)()
    }
}

#[cfg(feature = "fork")]
fn init_replay(rng: &mut TestRng) -> (Vec<TestCaseResult>, ForkOutput) {
    use test_runner::replay::{open_file, Replay, ReplayFileStatus::*};

    if let Some(path) = env::var_os(ENV_FORK_FILE) {
        let mut file = open_file(&path).expect("Failed to open replay file");
        let loaded = Replay::parse_from(&mut file)
                        .expect("Failed to read replay file");
        match loaded {
            InProgress(replay) => {
                rng.set_seed(replay.seed);
                (replay.steps, ForkOutput { file: Some(file) })
            },

            Terminated(_) =>
                panic!("Replay file for child process is terminated?"),

            Corrupt =>
                panic!("Replay file for child process is corrupt"),
        }
    } else {
        (vec![], ForkOutput::empty())
    }
}

#[cfg(not(feature = "fork"))]
fn init_replay(_rng: &mut TestRng) -> (iter::Empty<TestCaseResult>, ForkOutput) {
    (iter::empty(), ForkOutput::empty())
}

#[cfg(feature = "fork")]
fn await_child_without_timeout(child: &mut rusty_fork::ChildWrapper)
                               -> (Option<TestCaseError>, Option<u64>) {
    let status = child.wait().expect("Failed to wait for child process");

    if status.success() {
        (None, None)
    } else {
        (Some(TestCaseError::fail(format!(
            "Child process exited with {}", status))), None)
    }
}

#[cfg(all(feature = "fork", not(feature = "timeout")))]
fn await_child(child: &mut rusty_fork::ChildWrapper,
               _: &mut tempfile::NamedTempFile,
               _timeout: u32)
               -> (Option<TestCaseError>, Option<u64>) {
    await_child_without_timeout(child)
}

#[cfg(all(feature = "fork", feature = "timeout"))]
fn await_child(child: &mut rusty_fork::ChildWrapper,
               forkfile: &mut tempfile::NamedTempFile,
               timeout: u32)
               -> (Option<TestCaseError>, Option<u64>) {
    use std::time::Duration;

    if 0 == timeout {
        return await_child_without_timeout(child);
    }

    // The child can run for longer than the timeout since it may run
    // multiple tests. Each time the timeout expires, we check whether the
    // file has grown larger. If it has, we allow the child to keep running
    // until the next timeout.
    let mut last_forkfile_len = forkfile.as_file().metadata()
        .map(|md| md.len()).unwrap_or(0);

    loop {
        if let Some(status) =
            child.wait_timeout(Duration::from_millis(timeout.into()))
            .expect("Failed to wait for child process")
        {
            if status.success() {
                return (None, None);
            } else {
                return (Some(TestCaseError::fail(format!(
                    "Child process exited with {}", status))), None);
            }
        }

        let current_len = forkfile.as_file().metadata()
            .map(|md| md.len()).unwrap_or(0);
        // If we've gone a full timeout period without the file growing,
        // fail the test and kill the child.
        if current_len <= last_forkfile_len {
            return (Some(TestCaseError::fail(format!(
                "Timed out waiting for child process"))), Some(current_len));
        } else {
            last_forkfile_len = current_len;
        }
    }
}

#[cfg(test)]
mod test {
    use std::cell::Cell;
    use std::fs;

    use super::*;
    use test_runner::FileFailurePersistence;
    use strategy::Strategy;

    #[test]
    fn gives_up_after_too_many_rejections() {
        let config = Config::default();
        let mut runner = TestRunner::new(config.clone());
        let runs = Cell::new(0);
        let result = runner.run(&(0u32..), |_| {
            runs.set(runs.get() + 1);
            Err(TestCaseError::reject("reject"))
        });
        match result {
            Err(TestError::Abort(_)) => (),
            e => panic!("Unexpected result: {:?}", e),
        }
        assert_eq!(config.max_global_rejects + 1, runs.get());
    }

    #[test]
    fn test_pass() {
        let mut runner = TestRunner::default();
        let result = runner.run(&(1u32..), |v| { assert!(v > 0); Ok(()) });
        assert_eq!(Ok(()), result);
    }

    #[test]
    fn test_fail_via_result() {
        let mut runner = TestRunner::new(Config {
            failure_persistence: None,
            .. Config::default()
        });
        let result = runner.run(
            &(0u32..10u32), |v| {
                if v < 5 {
                    Ok(())
                } else {
                    Err(TestCaseError::fail("not less than 5"))
                }
            });

        assert_eq!(Err(TestError::Fail("not less than 5".into(), 5)), result);
    }

    #[test]
    fn test_fail_via_panic() {
        let mut runner = TestRunner::new(Config {
            failure_persistence: None,
            .. Config::default()
        });
        let result = runner.run(&(0u32..10u32), |v| {
            assert!(v < 5, "not less than 5");
            Ok(())
        });
        assert_eq!(Err(TestError::Fail("not less than 5".into(), 5)), result);
    }

    #[derive(Clone, Copy, PartialEq)]
    struct PoorlyBehavedDebug(i32);
    impl fmt::Debug for PoorlyBehavedDebug {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "\r\n{:?}\r\n", self.0)
        }
    }

    #[test]
    fn failing_cases_persisted_and_reloaded() {
        const FILE: &'static str = "persistence-test.txt";
        let _ = fs::remove_file(FILE);

        let max = 10_000_000i32;
        let input = (0i32..max).prop_map(PoorlyBehavedDebug);
        let config = Config {
            failure_persistence: Some(Box::new(
                FileFailurePersistence::Direct(FILE)
            )),
            .. Config::default()
        };

        // First test with cases that fail above half max, and then below half
        // max, to ensure we can correctly parse both lines of the persistence
        // file.
        let first_sub_failure = {
            TestRunner::new(config.clone()).run(&input, |v| {
                if v.0 < max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too big".into()))
                }
            }).expect_err("didn't fail?")
        };
        let first_super_failure = {
            TestRunner::new(config.clone()).run(&input, |v| {
                if v.0 >= max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too small".into()))
                }
            }).expect_err("didn't fail?")
        };
        let second_sub_failure = {
            TestRunner::new(config.clone()).run(&input, |v| {
                if v.0 < max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too big".into()))
                }
            }).expect_err("didn't fail?")
        };
        let second_super_failure = {
            TestRunner::new(config.clone()).run(&input, |v| {
                if v.0 >= max/2 {
                    Ok(())
                } else {
                    Err(TestCaseError::Fail("too small".into()))
                }
            }).expect_err("didn't fail?")
        };

        assert_eq!(first_sub_failure, second_sub_failure);
        assert_eq!(first_super_failure, second_super_failure);
    }

    #[test]
    fn new_rng_makes_separate_rng() {
        use rand::Rng;
        let mut runner = TestRunner::default();
        let from_1 = runner.new_rng().gen::<Seed>();
        let from_2 = runner.rng().gen::<Seed>();
        assert_ne!(from_1, from_2);
    }

    #[cfg(feature = "fork")]
    #[test]
    fn run_successful_test_in_fork() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            test_name: Some(concat!(
                module_path!(), "::run_successful_test_in_fork")),
            .. Config::default()
        });

        assert!(runner.run(&(0u32..1000), |_| Ok(())).is_ok());
    }

    #[cfg(feature = "fork")]
    #[test]
    fn normal_failure_in_fork_results_in_correct_failure() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            test_name: Some(concat!(
                module_path!(), "::normal_failure_in_fork_results_in_correct_failure")),
            .. Config::default()
        });

        let failure = runner.run(&(0u32..1000), |v| {
            prop_assert!(v < 500);
            Ok(())
        }).err().unwrap();

        match failure {
            TestError::Fail(_, value) => assert_eq!(500, value),
            failure => panic!("Unexpected failure: {:?}", failure),
        }
    }

    #[cfg(feature = "fork")]
    #[test]
    fn nonsuccessful_exit_finds_correct_failure() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            test_name: Some(concat!(
                module_path!(), "::nonsuccessful_exit_finds_correct_failure")),
            .. Config::default()
        });

        let failure = runner.run(&(0u32..1000), |v| {
            if v >= 500 {
                ::std::process::exit(1);
            }
            Ok(())
        }).err().unwrap();

        match failure {
            TestError::Fail(_, value) => assert_eq!(500, value),
            failure => panic!("Unexpected failure: {:?}", failure),
        }
    }

    #[cfg(feature = "fork")]
    #[test]
    fn spurious_exit_finds_correct_failure() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            test_name: Some(concat!(
                module_path!(), "::spurious_exit_finds_correct_failure")),
            .. Config::default()
        });

        let failure = runner.run(&(0u32..1000), |v| {
            if v >= 500 {
                ::std::process::exit(0);
            }
            Ok(())
        }).err().unwrap();

        match failure {
            TestError::Fail(_, value) => assert_eq!(500, value),
            failure => panic!("Unexpected failure: {:?}", failure),
        }
    }

    #[cfg(feature = "timeout")]
    #[test]
    fn long_sleep_timeout_finds_correct_failure() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            timeout: 500,
            test_name: Some(concat!(
                module_path!(), "::long_sleep_timeout_finds_correct_failure")),
            .. Config::default()
        });

        let failure = runner.run(&(0u32..1000), |v| {
            if v >= 500 {
                ::std::thread::sleep(::std::time::Duration::from_millis(10_000));
            }
            Ok(())
        }).err().unwrap();

        match failure {
            TestError::Fail(_, value) => assert_eq!(500, value),
            failure => panic!("Unexpected failure: {:?}", failure),
        }
    }

    #[cfg(feature = "timeout")]
    #[test]
    fn mid_sleep_timeout_finds_correct_failure() {
        let mut runner = TestRunner::new(Config {
            fork: true,
            timeout: 500,
            test_name: Some(concat!(
                module_path!(), "::mid_sleep_timeout_finds_correct_failure")),
            .. Config::default()
        });

        let failure = runner.run(&(0u32..1000), |v| {
            if v >= 500 {
                // Sleep a little longer than the timeout. This means that
                // sometimes the test case itself will return before the parent
                // process has noticed the child is timing out, so it's up to
                // the child to mark it as a failure.
                ::std::thread::sleep(::std::time::Duration::from_millis(600));
            } else {
                // Sleep a bit so that the parent and child timing don't stay
                // in sync.
                ::std::thread::sleep(::std::time::Duration::from_millis(100))
            }
            Ok(())
        }).err().unwrap();

        match failure {
            TestError::Fail(_, value) => assert_eq!(500, value),
            failure => panic!("Unexpected failure: {:?}", failure),
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn duplicate_tests_not_run_with_basic_result_cache() {
        use std::cell::{Cell, RefCell};
        use std::collections::HashSet;
        use std::rc::Rc;

        for _ in 0..256 {
            let mut runner = TestRunner::new(Config {
                failure_persistence: None,
                result_cache: ::test_runner::result_cache::basic_result_cache,
                .. Config::default()
            });
            let pass = Rc::new(Cell::new(true));
            let seen = Rc::new(RefCell::new(HashSet::new()));
            let result = runner.run(
                &(0u32..65536u32).prop_map(|v| v % 10),
                |val| {
                    if !seen.borrow_mut().insert(val) {
                        println!("Value {} seen more than once", val);
                        pass.set(false);
                    }

                    prop_assert!(val <= 5);
                    Ok(())
                });

            assert!(pass.get());
            if let Err(TestError::Fail(_, val)) = result {
                assert_eq!(6, val);
            } else {
                panic!("Incorrect result: {:?}", result);
            }
        }
    }
}
