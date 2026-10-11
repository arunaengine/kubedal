//! Pause points for tests that need an exact order of concurrent operations
//! (kubedal addition).
//!
//! Code calls [`PausePoints::hit`] at a named place. Normally nothing is armed and
//! the call returns at once. A test arms a point, waits until a task reaches it,
//! runs another operation, and then lets the paused task go on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

/// One armed pause point.
pub struct Pause {
    reached: Semaphore,
    resume: Semaphore,
}

impl Pause {
    /// Wait until a task stopped at this point.
    pub async fn reached(&self) {
        if let Ok(permit) = self.reached.acquire().await {
            permit.forget();
        }
    }

    /// Let the stopped task go on.
    pub fn resume(&self) {
        self.resume.add_permits(1);
    }
}

/// Named pause points. Each armed point stops the next task that hits it once.
/// Failure points make the next check at a named place report a failure once.
#[derive(Default)]
pub struct PausePoints {
    points: Mutex<HashMap<&'static str, Arc<Pause>>>,
    failures: Mutex<Vec<&'static str>>,
    crash_at: Mutex<Option<&'static str>>,
}

/// Exit code of a process that stopped at a crash point.
pub const CRASH_EXIT_CODE: i32 = 86;

impl PausePoints {
    /// Arm the point `name`. The next task that hits it stops there.
    pub fn arm(&self, name: &'static str) -> Arc<Pause> {
        let pause = Arc::new(Pause {
            reached: Semaphore::new(0),
            resume: Semaphore::new(0),
        });
        self.points
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name, pause.clone());
        pause
    }

    /// Make the next [`PausePoints::should_fail`] for `name` return true.
    pub fn fail_once(&self, name: &'static str) {
        self.failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(name);
    }

    /// True once after [`PausePoints::fail_once`] armed `name`.
    pub fn should_fail(&self, name: &'static str) -> bool {
        let mut failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        match failures.iter().position(|f| *f == name) {
            Some(i) => {
                failures.remove(i);
                true
            }
            None => false,
        }
    }

    /// Make the process exit at once when it reaches the point `name`. Crash
    /// tests run this in a child process and check the spool folder afterwards.
    pub fn crash_at(&self, name: &'static str) {
        *self.crash_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(name);
    }

    /// Stop here if the point `name` is armed, until the test resumes it.
    /// Exit the process if `name` is the crash point.
    pub async fn hit(&self, name: &'static str) {
        if *self.crash_at.lock().unwrap_or_else(|p| p.into_inner()) == Some(name) {
            std::process::exit(CRASH_EXIT_CODE);
        }
        let pause = self
            .points
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        if let Some(pause) = pause {
            pause.reached.add_permits(1);
            if let Ok(permit) = pause.resume.acquire().await {
                permit.forget();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_task_stops_until_it_is_resumed() {
        let points = Arc::new(PausePoints::default());
        let pause = points.arm("here");
        let task = {
            let points = points.clone();
            tokio::spawn(async move {
                points.hit("here").await;
                // Unarmed points do not stop.
                points.hit("here").await;
            })
        };
        pause.reached().await;
        assert!(!task.is_finished());
        pause.resume();
        task.await.unwrap();
    }
}
