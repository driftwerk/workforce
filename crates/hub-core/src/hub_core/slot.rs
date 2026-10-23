//! A subsystem the service carries for the code that wires and reads it,
//! without naming its type.

use std::any::Any;
use std::sync::{Arc, OnceLock};

/// Set once, read back as what it was set as. The first setting stands. A
/// read as another type than the one set is a wiring bug: it is logged and
/// fails debug builds, and reads `None`.
#[derive(Default)]
pub struct Slot(OnceLock<Arc<dyn Any + Send + Sync>>);

impl Slot {
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    pub fn set<T: Any + Send + Sync>(&self, value: Arc<T>) {
        let _ = self.0.set(value);
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let held = Arc::clone(self.0.get()?);
        match held.downcast::<T>() {
            Ok(value) => Some(value),
            Err(_) => {
                let wanted = std::any::type_name::<T>();
                tracing::error!(wanted, "slot read as a type it was not set as");
                debug_assert!(false, "slot read as {wanted}, which it was not set as");
                None
            }
        }
    }
}
