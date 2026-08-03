//! Types related to provider events.
//!
//! See the [spec](https://openfeature.dev/specification/sections/events).

use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use typed_builder::TypedBuilder;

use crate::api::event_registry::EventRegistry;
use crate::{EvaluationErrorCode, Value};

// ============================================================
//  ProviderEventType
// ============================================================

/// The type of a provider event.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProviderEventType {
    /// The provider is ready to perform flag evaluations.
    Ready,

    /// The provider signaled an error.
    Error,

    /// The provider's cached state is no longer valid and may not be up-to-date with the source
    /// of truth.
    Stale,

    /// A change was made to the backend flag configuration.
    ConfigurationChanged,
}

impl Display for ProviderEventType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Ready => "PROVIDER_READY",
            Self::Error => "PROVIDER_ERROR",
            Self::Stale => "PROVIDER_STALE",
            Self::ConfigurationChanged => "PROVIDER_CONFIGURATION_CHANGED",
        };
        write!(f, "{value}")
    }
}

// ============================================================
//  EventDetails
// ============================================================

/// The details of a provider event, passed to registered event handlers.
#[derive(Clone, Default, Debug, TypedBuilder)]
pub struct EventDetails {
    /// The name of the provider that emitted the event.
    #[builder(setter(into))]
    pub provider_name: String,

    /// An informative message about the event.
    #[builder(default, setter(strip_option, into))]
    pub message: Option<String>,

    /// The error code, populated for error events.
    #[builder(default, setter(strip_option))]
    pub error_code: Option<EvaluationErrorCode>,

    /// The flag keys affected by a configuration change.
    #[builder(default)]
    pub flags_changed: Vec<String>,

    /// Arbitrary metadata associated with the event.
    #[builder(default)]
    pub event_metadata: HashMap<String, Value>,
}

// ============================================================
//  EventHandler
// ============================================================

/// A function to be executed when the associated provider event is emitted.
pub type EventHandler = Arc<dyn Fn(&EventDetails) + Send + Sync>;

/// An opaque identifier of a registered event handler.
/// Use it to remove the handler via `remove_handler`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EventHandlerId(pub(crate) u64);

// ============================================================
//  EventEmitter
// ============================================================

/// A handle given to a provider (via [`crate::provider::FeatureProvider::attach_emitter`]) that
/// allows it to emit events to the SDK while it is registered.
///
/// When the provider is replaced or the API is shut down, the emitter is deactivated and
/// subsequent calls to [`EventEmitter::emit`] become no-ops.
#[derive(Clone)]
pub struct EventEmitter {
    domain: String,
    registry: EventRegistry,
    active: Arc<AtomicBool>,
    armed: Arc<AtomicBool>,
}

impl EventEmitter {
    pub(crate) fn new(domain: String, registry: EventRegistry, active: Arc<AtomicBool>) -> Self {
        Self {
            domain,
            registry,
            active,
            armed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Arm the emitter so that subsequent [`EventEmitter::emit`] calls are delivered.
    ///
    /// The SDK arms the emitter only *after* it has dispatched its own terminal
    /// `PROVIDER_READY`/`PROVIDER_ERROR` event derived from the provider's initialization
    /// outcome. Events a provider emits from within `initialize` are therefore ignored: the SDK
    /// owns the initial readiness event, so a provider that also signals readiness during
    /// initialization cannot cause `PROVIDER_READY`/`PROVIDER_ERROR` handlers to run twice
    /// (spec 5.3.1 / 5.3.2).
    pub(crate) fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Emit an event of type `event_type` with given `details`.
    ///
    /// The associated event handlers are executed on the calling task before this function
    /// returns. Does nothing if the provider owning this emitter is no longer registered, or if
    /// the SDK has not yet finished initializing it (see [`EventEmitter::arm`]).
    pub async fn emit(&self, event_type: ProviderEventType, details: EventDetails) {
        if self.armed.load(Ordering::Acquire) && self.active.load(Ordering::Acquire) {
            self.registry
                .dispatch(&self.domain, event_type, &details, &self.active)
                .await;
        }
    }
}

impl std::fmt::Debug for EventEmitter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventEmitter")
            .field("domain", &self.domain)
            .field("active", &self.active.load(Ordering::Acquire))
            .field("armed", &self.armed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
