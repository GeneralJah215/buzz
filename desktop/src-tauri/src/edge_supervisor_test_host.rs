//! The fake [`SupervisorHost`] every supervisor test drives, plus the fixtures
//! they share.
//!
//! Shared by `edge_supervisor_tests.rs` and `edge_supervisor_loop_tests.rs`.
//! It records every call and performs none of them: no test registers a
//! scheduled task, starts a process, sleeps, or writes outside a `tempfile`
//! directory. `WindowsHost` is never constructed.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    LedgerLoad, SidecarHealth, SupervisorEnv, SupervisorHost, SupervisorLedger, TaskRegistration,
    TaskSpec, TASK_NAME,
};

pub const EXE: &str = r"C:\Program Files\Buzz\buzz-edge.exe";
pub const VERSION: &str = "0.5.5";

pub fn exe_path() -> PathBuf {
    PathBuf::from(EXE)
}

pub fn spec() -> TaskSpec {
    TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: exe_path(),
        app_version: VERSION.to_string(),
    }
}

pub fn env_on() -> SupervisorEnv {
    SupervisorEnv {
        edge_relay_url: Some("ws://127.0.0.1:7777".to_string()),
        sidecar_exe: exe_path(),
        app_version: VERSION.to_string(),
    }
}

pub fn env_off() -> SupervisorEnv {
    SupervisorEnv {
        edge_relay_url: None,
        ..env_on()
    }
}

pub fn registered_correctly() -> TaskRegistration {
    TaskRegistration::Registered {
        command_line: format!("\"{EXE}\""),
    }
}

/// Records every call and performs none of them. `query_task`, `probe_health`
/// and `store_ledger` return scripted sequences so a repair-then-verify cycle,
/// a cold-start-then-healthy spawn, or a ledger that cannot be written can each
/// be driven exactly.
pub struct FakeHost {
    queries: RefCell<Vec<TaskRegistration>>,
    probes: RefCell<Vec<SidecarHealth>>,
    store_results: RefCell<Vec<Result<(), String>>>,
    pub register_result: Result<(), String>,
    pub spawn_result: Result<(), String>,
    pub sidecar_present: bool,
    ledger: RefCell<SupervisorLedger>,
    pub ledger_warning: Option<String>,
    calls: RefCell<Vec<String>>,
    stored: RefCell<Vec<SupervisorLedger>>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    pub fn new() -> Self {
        Self {
            queries: RefCell::new(vec![registered_correctly()]),
            probes: RefCell::new(vec![SidecarHealth::Healthy]),
            store_results: RefCell::new(vec![Ok(())]),
            register_result: Ok(()),
            spawn_result: Ok(()),
            sidecar_present: true,
            ledger: RefCell::new(SupervisorLedger::default()),
            ledger_warning: None,
            calls: RefCell::new(Vec::new()),
            stored: RefCell::new(Vec::new()),
        }
    }

    pub fn with_queries(mut self, queries: Vec<TaskRegistration>) -> Self {
        self.queries = RefCell::new(queries);
        self
    }

    pub fn with_probes(mut self, probes: Vec<SidecarHealth>) -> Self {
        self.probes = RefCell::new(probes);
        self
    }

    /// Script what `store_ledger` returns, in call order. This is the branch
    /// the old fake could not reach at all: it hardcoded `Ok(())`, so the
    /// "ledger cannot be written" loop had no test and no behaviour.
    pub fn with_store_results(mut self, results: Vec<Result<(), String>>) -> Self {
        self.store_results = RefCell::new(results);
        self
    }

    pub fn with_missing_sidecar(mut self) -> Self {
        self.sidecar_present = false;
        self
    }

    pub fn with_ledger(self, ledger: SupervisorLedger) -> Self {
        *self.ledger.borrow_mut() = ledger;
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    pub fn count_calls(&self, prefix: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    /// The last ledger the supervisor *attempted* to store.
    pub fn final_ledger(&self) -> SupervisorLedger {
        self.stored
            .borrow()
            .last()
            .cloned()
            .expect("a ledger should have been stored")
    }

    /// What actually survived: writes that returned `Err` do not change it.
    /// This is what the next launch would load.
    pub fn persisted_ledger(&self) -> SupervisorLedger {
        self.ledger.borrow().clone()
    }

    /// Pop the next scripted value, repeating the last one forever so a test
    /// only has to script the steps it cares about.
    fn next<T: Clone>(queue: &RefCell<Vec<T>>) -> T {
        let mut queue = queue.borrow_mut();
        if queue.len() > 1 {
            queue.remove(0)
        } else {
            queue.first().cloned().expect("scripted value")
        }
    }
}

impl SupervisorHost for FakeHost {
    fn query_task(&self, task_name: &str) -> TaskRegistration {
        self.calls.borrow_mut().push(format!("query:{task_name}"));
        Self::next(&self.queries)
    }

    fn register_task(&self, spec: &TaskSpec) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("register:{}", spec.task_name));
        self.register_result.clone()
    }

    fn probe_health(&self) -> SidecarHealth {
        self.calls.borrow_mut().push("probe".to_string());
        Self::next(&self.probes)
    }

    fn spawn_sidecar(&self, exe: &Path) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("spawn:{}", exe.display()));
        self.spawn_result.clone()
    }

    fn sidecar_exists(&self, _exe: &Path) -> bool {
        self.sidecar_present
    }

    /// Records the wait and returns immediately. A test that spent
    /// `SPAWN_CONFIRM_INTERVAL` for real would be a test nobody runs.
    fn wait_before_reprobe(&self, delay: Duration) {
        self.calls.borrow_mut().push(format!("wait:{delay:?}"));
    }

    fn load_ledger(&self) -> LedgerLoad {
        self.calls.borrow_mut().push("load_ledger".to_string());
        LedgerLoad {
            ledger: self.ledger.borrow().clone(),
            warning: self.ledger_warning.clone(),
        }
    }

    fn store_ledger(&self, ledger: &SupervisorLedger) -> Result<(), String> {
        self.calls.borrow_mut().push("store_ledger".to_string());
        self.stored.borrow_mut().push(ledger.clone());
        let result = Self::next(&self.store_results);
        if result.is_ok() {
            *self.ledger.borrow_mut() = ledger.clone();
        }
        result
    }
}
