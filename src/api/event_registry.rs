use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::provider::ProviderStatus;
use crate::{EvaluationErrorCode, EventDetails, EventHandler, EventHandlerId, ProviderEventType};

// ============================================================
//  EventRegistry
// ============================================================

/// Internal storage of event handlers and SDK-tracked provider statuses.
///
/// It is shared (via cheap clones) between the API, the provider registry and all clients, so
/// that events emitted for a domain reach both API-level handlers and the handlers of clients
/// bound to that domain.
#[derive(Clone, Default)]
pub(crate) struct EventRegistry(Arc<RwLock<EventRegistryInner>>);

/// Scope of a handler: `None` for API-level handlers, `Some(domain)` for client-level handlers.
type HandlerScope = Option<String>;

#[derive(Default)]
struct EventRegistryInner {
    next_id: u64,
    handlers: HashMap<(HandlerScope, ProviderEventType), Vec<(EventHandlerId, EventHandler)>>,

    /// SDK-tracked state per domain. The default provider lives under `""`.
    domains: HashMap<String, DomainState>,

    /// Domains that have a named provider bound to them.
    bound_domains: HashSet<String>,
}

struct DomainState {
    status: ProviderStatus,
    provider_name: String,
    active_token: Arc<AtomicBool>,

    /// Details of the event that last set `status`, replayed to handlers that are registered
    /// once the provider is already in that state (spec 5.3.3).
    last_details: Option<EventDetails>,
}

impl EventRegistry {
    /// Register a handler for `event_type` within the given `scope`.
    ///
    /// If the relevant provider is already in the state corresponding to `event_type`, the
    /// handler is invoked immediately (spec 5.3.3).
    pub async fn add_handler(
        &self,
        scope: HandlerScope,
        event_type: ProviderEventType,
        handler: EventHandler,
    ) -> EventHandlerId {
        let (id, replay) = {
            let mut inner = self.0.write().await;

            inner.next_id += 1;
            let id = EventHandlerId(inner.next_id);

            // Determine whether the provider is already in the state associated with
            // `event_type`. For API-level handlers the default provider's state is consulted.
            let domain = match &scope {
                Some(domain) if inner.bound_domains.contains(domain) => domain.as_str(),
                _ => "",
            };
            let replay = inner.domains.get(domain).and_then(|state| {
                if event_matches_status(event_type, state.status) {
                    Some(state.last_details.clone().unwrap_or_else(|| {
                        EventDetails::builder()
                            .provider_name(state.provider_name.clone())
                            .build()
                    }))
                } else {
                    None
                }
            });

            inner
                .handlers
                .entry((scope, event_type))
                .or_default()
                .push((id, handler.clone()));

            (id, replay)
        };

        // Run outside the lock, as handlers may interact with the SDK (spec 5.3.3).
        if let Some(details) = replay {
            invoke_handler(&handler, &details);
        }

        id
    }

    /// Remove the handler registered under the given `id`, if any.
    pub async fn remove_handler(&self, id: EventHandlerId) {
        let mut inner = self.0.write().await;

        for handlers in inner.handlers.values_mut() {
            handlers.retain(|(handler_id, _)| *handler_id != id);
        }
    }

    /// Return the SDK-tracked status of the provider serving the given `domain`.
    ///
    /// Falls back to the default provider's status if no provider is bound to `domain`.
    pub async fn provider_status(&self, domain: &str) -> ProviderStatus {
        let inner = self.0.read().await;

        let domain = if inner.bound_domains.contains(domain) {
            domain
        } else {
            ""
        };

        inner
            .domains
            .get(domain)
            .map_or(ProviderStatus::NotReady, |state| state.status)
    }

    /// Record that a new provider is being registered for `domain`, deactivating the previous
    /// provider's event emitter. Returns the activity token for the new provider's emitter.
    pub async fn on_provider_set(&self, domain: &str, provider_name: &str) -> Arc<AtomicBool> {
        let mut inner = self.0.write().await;

        if !domain.is_empty() {
            inner.bound_domains.insert(domain.to_string());
        }

        let token = Arc::new(AtomicBool::new(true));

        if let Some(old) = inner.domains.insert(
            domain.to_string(),
            DomainState {
                status: ProviderStatus::NotReady,
                provider_name: provider_name.to_string(),
                active_token: token.clone(),
                last_details: None,
            },
        ) {
            old.active_token.store(false, Ordering::Release);
        }

        token
    }

    /// Dispatch an event emitted by the provider serving `domain`: update the tracked provider
    /// status (spec 5.3.5) and execute the associated handlers (spec 5.1.2 / 5.1.3).
    ///
    /// `token` is the activity token of the provider on whose behalf the event is dispatched
    /// (as returned by [`EventRegistry::on_provider_set`]). If a newer provider has since been
    /// bound to `domain`, the token no longer matches the domain's current provider and the
    /// event is ignored, so that a slow-to-initialize provider that has already been superseded
    /// cannot clobber the current provider's status or notify handlers (spec 5.1.3 / 5.3.5).
    pub async fn dispatch(
        &self,
        domain: &str,
        event_type: ProviderEventType,
        details: &EventDetails,
        token: &Arc<AtomicBool>,
    ) {
        let handlers = {
            let mut inner = self.0.write().await;

            // Drop the event if its originating provider is no longer the one bound to `domain`.
            match inner.domains.get(domain) {
                Some(state) if Arc::ptr_eq(&state.active_token, token) => {}
                _ => return,
            }

            if let Some(status) = status_of_event(event_type, details) {
                if let Some(state) = inner.domains.get_mut(domain) {
                    state.status = status;
                    state.last_details = Some(details.clone());
                }
            }

            let mut handlers = Vec::new();

            // API-level handlers run for events from any provider.
            collect(&inner, &None, event_type, &mut handlers);

            if domain.is_empty() {
                // An event from the default provider reaches the handlers of every client
                // domain that has no dedicated provider bound to it. Only keys of this event
                // type are considered, so that each such domain is visited exactly once.
                let unbound: Vec<HandlerScope> = inner
                    .handlers
                    .keys()
                    .filter(|(_, key_event_type)| *key_event_type == event_type)
                    .filter_map(|(scope, _)| match scope {
                        Some(domain) if !inner.bound_domains.contains(domain) => {
                            Some(scope.clone())
                        }
                        _ => None,
                    })
                    .collect();

                for scope in unbound {
                    collect(&inner, &scope, event_type, &mut handlers);
                }
            } else {
                collect(&inner, &Some(domain.to_string()), event_type, &mut handlers);
            }

            handlers
        };

        // Handlers run outside the lock so that they may safely interact with the SDK.
        for handler in handlers {
            invoke_handler(&handler, details);
        }
    }

    /// Reset all provider state, deactivating every outstanding emitter.
    /// Registered handlers are retained.
    pub async fn clear(&self) {
        let mut inner = self.0.write().await;

        for state in inner.domains.values() {
            state.active_token.store(false, Ordering::Release);
        }

        inner.domains.clear();
        inner.bound_domains.clear();
    }
}

/// The provider status implied by an event, or `None` if the event does not affect it
/// (spec 5.3.5).
///
/// `PROVIDER_ERROR` maps to [`ProviderStatus::Fatal`] when the event carries the
/// [`EvaluationErrorCode::ProviderFatal`] error code, and to [`ProviderStatus::Error`] otherwise.
fn status_of_event(
    event_type: ProviderEventType,
    details: &EventDetails,
) -> Option<ProviderStatus> {
    match event_type {
        ProviderEventType::Ready => Some(ProviderStatus::Ready),
        ProviderEventType::Error => {
            if details.error_code == Some(EvaluationErrorCode::ProviderFatal) {
                Some(ProviderStatus::Fatal)
            } else {
                Some(ProviderStatus::Error)
            }
        }
        ProviderEventType::Stale => Some(ProviderStatus::Stale),
        ProviderEventType::ConfigurationChanged => None,
    }
}

/// Whether a handler for `event_type` should run immediately upon registration, given that the
/// associated provider is already in `status` (spec 5.3.3).
///
/// `PROVIDER_ERROR` is associated with both the `ERROR` and `FATAL` statuses, per the event to
/// status mapping of spec 5.3.5. `PROVIDER_CONFIGURATION_CHANGED` has no associated status and so
/// never runs on registration.
fn event_matches_status(event_type: ProviderEventType, status: ProviderStatus) -> bool {
    match event_type {
        ProviderEventType::Ready => status == ProviderStatus::Ready,
        ProviderEventType::Error => {
            status == ProviderStatus::Error || status == ProviderStatus::Fatal
        }
        ProviderEventType::Stale => status == ProviderStatus::Stale,
        ProviderEventType::ConfigurationChanged => false,
    }
}

fn collect(
    inner: &EventRegistryInner,
    scope: &HandlerScope,
    event_type: ProviderEventType,
    out: &mut Vec<EventHandler>,
) {
    if let Some(handlers) = inner.handlers.get(&(scope.clone(), event_type)) {
        out.extend(handlers.iter().map(|(_, handler)| handler.clone()));
    }
}

/// Invoke a handler, containing any panic so that one erroring handler does not prevent the
/// execution of others (spec 5.2.5).
fn invoke_handler(handler: &EventHandler, details: &EventDetails) {
    if catch_unwind(AssertUnwindSafe(|| handler(details))).is_err() {
        log::error!(
            "an event handler panicked while handling an event of provider '{}'",
            details.provider_name
        );
    }
}

// ============================================================
//  Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use spec::spec;

    use async_trait::async_trait;

    use super::*;
    use crate::provider::{
        FeatureProvider, MockFeatureProvider, ProviderMetadata, ResolutionDetails,
    };
    use crate::{
        EvaluationContext, EvaluationError, EvaluationErrorCode, EvaluationResult, EventEmitter,
        OpenFeature, ProviderEventType, StructValue,
    };

    fn provider(name: &str) -> MockFeatureProvider {
        let mut provider = MockFeatureProvider::new();
        provider.expect_initialize().returning(|_| Ok(()));
        provider.expect_attach_emitter().return_const(());
        provider
            .expect_metadata()
            .return_const(ProviderMetadata::new(name));
        provider
    }

    /// A mock provider that captures the [`EventEmitter`] handed to it by the SDK.
    fn emitting_provider(name: &str) -> (MockFeatureProvider, Arc<Mutex<Option<EventEmitter>>>) {
        let mut provider = MockFeatureProvider::new();
        provider.expect_initialize().returning(|_| Ok(()));
        provider
            .expect_metadata()
            .return_const(ProviderMetadata::new(name));

        let emitter = Arc::new(Mutex::new(None));
        let captured = emitter.clone();
        provider.expect_attach_emitter().returning(move |e| {
            *captured.lock().unwrap() = Some(e);
        });

        (provider, emitter)
    }

    fn counting_handler(count: &Arc<AtomicUsize>) -> impl Fn(&EventDetails) + Send + Sync {
        let count = count.clone();
        move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A misbehaving provider that emits `PROVIDER_READY` itself from within `initialize`, in
    /// addition to the terminal readiness event the SDK derives from the initialization outcome.
    struct EmitReadyDuringInitProvider {
        metadata: ProviderMetadata,
        emitter: Mutex<Option<EventEmitter>>,
    }

    impl EmitReadyDuringInitProvider {
        fn new(name: &str) -> Self {
            Self {
                metadata: ProviderMetadata::new(name),
                emitter: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl FeatureProvider for EmitReadyDuringInitProvider {
        fn attach_emitter(&mut self, emitter: EventEmitter) {
            *self.emitter.lock().unwrap() = Some(emitter);
        }

        async fn initialize(
            &mut self,
            _context: &EvaluationContext,
        ) -> Result<(), EvaluationError> {
            let emitter = self.emitter.lock().unwrap().clone().unwrap();
            emitter
                .emit(
                    ProviderEventType::Ready,
                    EventDetails::builder()
                        .provider_name(self.metadata.name.clone())
                        .build(),
                )
                .await;
            Ok(())
        }

        fn metadata(&self) -> &ProviderMetadata {
            &self.metadata
        }

        async fn resolve_bool_value(
            &self,
            _flag_key: &str,
            _context: &EvaluationContext,
        ) -> EvaluationResult<ResolutionDetails<bool>> {
            unreachable!("flag resolution is not exercised by this test")
        }

        async fn resolve_int_value(
            &self,
            _flag_key: &str,
            _context: &EvaluationContext,
        ) -> EvaluationResult<ResolutionDetails<i64>> {
            unreachable!("flag resolution is not exercised by this test")
        }

        async fn resolve_float_value(
            &self,
            _flag_key: &str,
            _context: &EvaluationContext,
        ) -> EvaluationResult<ResolutionDetails<f64>> {
            unreachable!("flag resolution is not exercised by this test")
        }

        async fn resolve_string_value(
            &self,
            _flag_key: &str,
            _context: &EvaluationContext,
        ) -> EvaluationResult<ResolutionDetails<String>> {
            unreachable!("flag resolution is not exercised by this test")
        }

        async fn resolve_struct_value(
            &self,
            _flag_key: &str,
            _context: &EvaluationContext,
        ) -> EvaluationResult<ResolutionDetails<StructValue>> {
            unreachable!("flag resolution is not exercised by this test")
        }
    }

    #[spec(
        number = "5.1.1",
        text = "The provider MAY define a mechanism for signaling the occurrence of one of a set of events, including PROVIDER_READY, PROVIDER_ERROR, PROVIDER_CONFIGURATION_CHANGED and PROVIDER_STALE, with a provider event details payload."
    )]
    #[tokio::test]
    async fn provider_can_emit_all_event_types() {
        let mut api = OpenFeature::default();

        let received = Arc::new(Mutex::new(Vec::new()));
        for event_type in [
            ProviderEventType::Ready,
            ProviderEventType::Error,
            ProviderEventType::Stale,
            ProviderEventType::ConfigurationChanged,
        ] {
            let received = received.clone();
            api.add_handler(event_type, move |_| {
                received.lock().unwrap().push(event_type);
            })
            .await;
        }

        let (provider, emitter) = emitting_provider("test");
        api.set_provider(provider).await.unwrap();
        let emitter = emitter.lock().unwrap().clone().unwrap();

        for event_type in [
            ProviderEventType::Error,
            ProviderEventType::Stale,
            ProviderEventType::ConfigurationChanged,
        ] {
            emitter
                .emit(
                    event_type,
                    EventDetails::builder().provider_name("test").build(),
                )
                .await;
        }

        // `Ready` was recorded when initialization succeeded, the rest were emitted manually.
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                ProviderEventType::Ready,
                ProviderEventType::Error,
                ProviderEventType::Stale,
                ProviderEventType::ConfigurationChanged,
            ]
        );
    }

    #[spec(
        number = "5.1.2",
        text = "When a provider signals the occurrence of a particular event, the associated client and API event handlers MUST run."
    )]
    #[spec(
        number = "5.2.1",
        text = "The client MUST provide a function for associating handler functions with a particular provider event type."
    )]
    #[spec(
        number = "5.2.2",
        text = "The API MUST provide a function for associating handler functions with a particular provider event type."
    )]
    #[tokio::test]
    async fn api_and_client_handlers_run() {
        let mut api = OpenFeature::default();

        let api_count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&api_count))
            .await;

        let client = api.create_named_client("domain");
        let client_count = Arc::new(AtomicUsize::new(0));
        client
            .add_handler(ProviderEventType::Ready, counting_handler(&client_count))
            .await;

        api.set_named_provider("domain", provider("test"))
            .await
            .unwrap();

        assert_eq!(api_count.load(Ordering::SeqCst), 1);
        assert_eq!(client_count.load(Ordering::SeqCst), 1);
    }

    #[spec(
        number = "5.1.3",
        text = "When a provider signals the occurrence of a particular event, event handlers on clients which are not associated with that provider MUST NOT run."
    )]
    #[tokio::test]
    async fn handlers_of_unassociated_clients_do_not_run() {
        let mut api = OpenFeature::default();

        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));

        let client_a = api.create_named_client("a");
        client_a
            .add_handler(ProviderEventType::Ready, counting_handler(&count_a))
            .await;

        let client_b = api.create_named_client("b");
        client_b
            .add_handler(ProviderEventType::Ready, counting_handler(&count_b))
            .await;

        // An event of the provider bound to "a" does not reach the handlers of "b".
        api.set_named_provider("a", provider("provider-a"))
            .await
            .unwrap();
        assert_eq!(count_a.load(Ordering::SeqCst), 1);
        assert_eq!(count_b.load(Ordering::SeqCst), 0);

        // An event of the default provider reaches "b" (not bound to a dedicated provider),
        // but not "a".
        api.set_provider(provider("default")).await.unwrap();
        assert_eq!(count_a.load(Ordering::SeqCst), 1);
        assert_eq!(count_b.load(Ordering::SeqCst), 1);
    }

    #[spec(
        number = "5.1.4",
        text = "PROVIDER_ERROR events SHOULD populate the provider event details's error message field."
    )]
    #[spec(
        number = "5.2.3",
        text = "The event details MUST contain the provider name associated with the event."
    )]
    #[spec(
        number = "5.2.4",
        text = "The handler function MUST accept a event details parameter."
    )]
    #[spec(
        number = "5.3.2",
        text = "If the provider's initialize function terminates abnormally, PROVIDER_ERROR handlers MUST run."
    )]
    #[tokio::test]
    async fn error_handlers_run_on_failed_initialization() {
        let mut api = OpenFeature::default();

        let received = Arc::new(Mutex::new(None));
        let captured = received.clone();
        api.add_handler(ProviderEventType::Error, move |details| {
            *captured.lock().unwrap() = Some(details.clone());
        })
        .await;

        let mut provider = MockFeatureProvider::new();
        provider.expect_attach_emitter().return_const(());
        provider
            .expect_metadata()
            .return_const(ProviderMetadata::new("failing"));
        provider.expect_initialize().returning(|_| {
            Err(EvaluationError::builder()
                .code(EvaluationErrorCode::ProviderNotReady)
                .message("connection refused")
                .build())
        });

        assert!(api.set_provider(provider).await.is_err());

        let details = received.lock().unwrap().clone().unwrap();
        assert_eq!(details.provider_name, "failing");
        assert_eq!(details.message.as_deref(), Some("connection refused"));
        assert_eq!(
            details.error_code,
            Some(EvaluationErrorCode::ProviderNotReady)
        );
        assert_eq!(api.provider_status().await, ProviderStatus::Error);
    }

    #[spec(
        number = "5.2.5",
        text = "If a handler function terminates abnormally, other handler functions MUST run."
    )]
    #[tokio::test]
    async fn other_handlers_run_if_one_panics() {
        let mut api = OpenFeature::default();

        api.add_handler(ProviderEventType::Ready, |_| {
            panic!("a misbehaving event handler");
        })
        .await;

        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&count))
            .await;

        api.set_provider(provider("test")).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[spec(
        number = "5.2.6",
        text = "Event handlers MUST persist across provider changes."
    )]
    #[tokio::test]
    async fn handlers_persist_across_provider_changes() {
        let mut api = OpenFeature::default();

        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&count))
            .await;

        api.set_provider(provider("first")).await.unwrap();
        api.set_provider(provider("second")).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[spec(
        number = "5.2.7",
        text = "The API and client MUST provide a function allowing the removal of event handlers."
    )]
    #[tokio::test]
    async fn handlers_can_be_removed() {
        let mut api = OpenFeature::default();

        let api_count = Arc::new(AtomicUsize::new(0));
        let id = api
            .add_handler(ProviderEventType::Ready, counting_handler(&api_count))
            .await;
        api.remove_handler(id).await;

        let client = api.create_client();
        let client_count = Arc::new(AtomicUsize::new(0));
        let id = client
            .add_handler(ProviderEventType::Ready, counting_handler(&client_count))
            .await;
        client.remove_handler(id).await;

        api.set_provider(provider("test")).await.unwrap();

        assert_eq!(api_count.load(Ordering::SeqCst), 0);
        assert_eq!(client_count.load(Ordering::SeqCst), 0);
    }

    #[spec(
        number = "5.3.1",
        text = "If the provider's initialize function terminates normally, PROVIDER_READY handlers MUST run."
    )]
    #[tokio::test]
    async fn ready_handlers_run_on_successful_initialization() {
        let mut api = OpenFeature::default();

        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&count))
            .await;

        api.set_provider(provider("test")).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(api.provider_status().await, ProviderStatus::Ready);
    }

    #[spec(
        number = "5.3.3",
        text = "Handlers attached after the provider is already in the associated state, MUST run immediately."
    )]
    #[tokio::test]
    async fn handlers_added_late_run_immediately() {
        let mut api = OpenFeature::default();
        api.set_provider(provider("test")).await.unwrap();

        // API-level handler for an already-ready default provider.
        let api_count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&api_count))
            .await;
        assert_eq!(api_count.load(Ordering::SeqCst), 1);

        // Client-level handler; the client's domain is served by the ready default provider.
        let client = api.create_named_client("unbound");
        let client_count = Arc::new(AtomicUsize::new(0));
        client
            .add_handler(ProviderEventType::Ready, counting_handler(&client_count))
            .await;
        assert_eq!(client_count.load(Ordering::SeqCst), 1);

        // Handlers for a state the provider is not in do not run.
        let error_count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Error, counting_handler(&error_count))
            .await;
        assert_eq!(error_count.load(Ordering::SeqCst), 0);
    }

    #[spec(
        number = "5.3.4.1",
        text = "While the provider's on context changed function is executing, associated RECONCILING handlers MUST run."
    )]
    #[test]
    fn static_context_not_applicable() {}

    #[spec(
        number = "5.3.5",
        text = "If the provider emits an event, the value of the client's provider status MUST be updated accordingly."
    )]
    #[tokio::test]
    async fn provider_status_updates_according_to_events() {
        let mut api = OpenFeature::default();

        let (provider, emitter) = emitting_provider("test");
        api.set_named_provider("domain", provider).await.unwrap();
        let emitter = emitter.lock().unwrap().clone().unwrap();

        let client = api.create_named_client("domain");
        assert_eq!(client.provider_status().await, ProviderStatus::Ready);

        emitter
            .emit(
                ProviderEventType::Stale,
                EventDetails::builder().provider_name("test").build(),
            )
            .await;
        assert_eq!(client.provider_status().await, ProviderStatus::Stale);

        // A configuration change does not affect the provider status.
        emitter
            .emit(
                ProviderEventType::ConfigurationChanged,
                EventDetails::builder().provider_name("test").build(),
            )
            .await;
        assert_eq!(client.provider_status().await, ProviderStatus::Stale);

        emitter
            .emit(
                ProviderEventType::Ready,
                EventDetails::builder().provider_name("test").build(),
            )
            .await;
        assert_eq!(client.provider_status().await, ProviderStatus::Ready);
    }

    #[tokio::test]
    async fn replaced_provider_emitter_is_deactivated() {
        let mut api = OpenFeature::default();

        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Stale, counting_handler(&count))
            .await;

        let (first, emitter) = emitting_provider("first");
        api.set_provider(first).await.unwrap();
        let stale_emitter = emitter.lock().unwrap().clone().unwrap();

        api.set_provider(provider("second")).await.unwrap();

        // The replaced provider's emitter must no longer dispatch events.
        stale_emitter
            .emit(
                ProviderEventType::Stale,
                EventDetails::builder().provider_name("first").build(),
            )
            .await;

        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(api.provider_status().await, ProviderStatus::Ready);
    }
    #[tokio::test]
    async fn each_handler_runs_once_per_event() {
        let mut api = OpenFeature::default();
        let client = api.create_named_client("unbound");

        // Handlers for several event types are registered on the same domain; a single event
        // must still invoke each matching handler exactly once.
        let ready_count = Arc::new(AtomicUsize::new(0));
        client
            .add_handler(ProviderEventType::Ready, counting_handler(&ready_count))
            .await;
        client.add_handler(ProviderEventType::Error, |_| {}).await;
        client.add_handler(ProviderEventType::Stale, |_| {}).await;

        api.set_provider(provider("test")).await.unwrap();

        assert_eq!(ready_count.load(Ordering::SeqCst), 1);
    }

    #[spec(
        number = "5.1.4",
        text = "PROVIDER_ERROR events SHOULD populate the provider event details's error message field."
    )]
    #[tokio::test]
    async fn late_handlers_receive_the_original_event_details() {
        let mut api = OpenFeature::default();

        let mut provider = MockFeatureProvider::new();
        provider.expect_attach_emitter().return_const(());
        provider
            .expect_metadata()
            .return_const(ProviderMetadata::new("failing"));
        provider.expect_initialize().returning(|_| {
            Err(EvaluationError::builder()
                .code(EvaluationErrorCode::ParseError)
                .message("malformed flag configuration")
                .build())
        });
        api.set_provider(provider).await.unwrap_err();

        // A handler registered after the failure still sees why it failed.
        let received = Arc::new(Mutex::new(None));
        let captured = received.clone();
        api.add_handler(ProviderEventType::Error, move |details| {
            *captured.lock().unwrap() = Some(details.clone());
        })
        .await;

        let details = received.lock().unwrap().clone().unwrap();
        assert_eq!(details.provider_name, "failing");
        assert_eq!(
            details.message.as_deref(),
            Some("malformed flag configuration")
        );
        assert_eq!(details.error_code, Some(EvaluationErrorCode::ParseError));
    }
    #[spec(
        number = "5.3.5",
        text = "When a provider emits an event, the SDK MUST update the provider status to the status associated with that event before invoking any event handlers for that event, so that handlers observe a consistent status."
    )]
    #[tokio::test]
    async fn error_with_fatal_code_yields_fatal_status() {
        let mut api = OpenFeature::default();

        let (provider, emitter) = emitting_provider("test");
        api.set_provider(provider).await.unwrap();
        let emitter = emitter.lock().unwrap().clone().unwrap();

        // A plain error maps to ERROR ...
        emitter
            .emit(
                ProviderEventType::Error,
                EventDetails::builder()
                    .provider_name("test")
                    .error_code(EvaluationErrorCode::ParseError)
                    .build(),
            )
            .await;
        assert_eq!(api.provider_status().await, ProviderStatus::Error);

        // ... while PROVIDER_FATAL maps to FATAL.
        emitter
            .emit(
                ProviderEventType::Error,
                EventDetails::builder()
                    .provider_name("test")
                    .error_code(EvaluationErrorCode::ProviderFatal)
                    .build(),
            )
            .await;
        assert_eq!(api.provider_status().await, ProviderStatus::Fatal);
    }

    #[spec(
        number = "5.3.3",
        text = "Handlers attached after the provider is already in the associated state, MUST run immediately."
    )]
    #[tokio::test]
    async fn error_handlers_run_immediately_in_fatal_state() {
        let mut api = OpenFeature::default();

        let mut provider = MockFeatureProvider::new();
        provider.expect_attach_emitter().return_const(());
        provider
            .expect_metadata()
            .return_const(ProviderMetadata::new("fatal"));
        provider.expect_initialize().returning(|_| {
            Err(EvaluationError::builder()
                .code(EvaluationErrorCode::ProviderFatal)
                .message("bad credentials")
                .build())
        });
        api.set_provider(provider).await.unwrap_err();
        assert_eq!(api.provider_status().await, ProviderStatus::Fatal);

        // FATAL is an associated state of PROVIDER_ERROR, so the handler still runs.
        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Error, counting_handler(&count))
            .await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[spec(
        number = "5.3.5",
        text = "When a provider emits an event, the SDK MUST update the provider status to the status associated with that event before invoking any event handlers for that event, so that handlers observe a consistent status."
    )]
    #[tokio::test]
    async fn handlers_observe_the_updated_status() {
        let events = EventRegistry::default();
        let token = events.on_provider_set("", "test").await;

        // The handler reads the tracked status while it is being invoked; it must already
        // reflect the event being dispatched.
        let observed = Arc::new(Mutex::new(None));
        let captured = observed.clone();
        let probe = events.clone();
        events
            .add_handler(
                None,
                ProviderEventType::Stale,
                Arc::new(move |_: &EventDetails| {
                    *captured.lock().unwrap() = Some(read_status_off_runtime(&probe));
                }),
            )
            .await;

        events
            .dispatch(
                "",
                ProviderEventType::Stale,
                &EventDetails::builder().provider_name("test").build(),
                &token,
            )
            .await;

        assert_eq!(*observed.lock().unwrap(), Some(ProviderStatus::Stale));
    }

    #[spec(
        number = "5.3.1",
        text = "If the provider's initialize function terminates normally, PROVIDER_READY handlers MUST run."
    )]
    #[tokio::test]
    async fn provider_emitting_ready_during_init_does_not_double_fire() {
        let mut api = OpenFeature::default();

        let count = Arc::new(AtomicUsize::new(0));
        api.add_handler(ProviderEventType::Ready, counting_handler(&count))
            .await;

        // The provider signals PROVIDER_READY itself while initializing; the SDK owns the
        // terminal readiness event, so handlers must still run exactly once (not twice).
        api.set_provider(EmitReadyDuringInitProvider::new("eager"))
            .await
            .unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(api.provider_status().await, ProviderStatus::Ready);
    }

    #[spec(
        number = "5.1.3",
        text = "When a provider signals the occurrence of a particular event, event handlers on clients which are not associated with that provider MUST NOT run."
    )]
    #[tokio::test]
    async fn superseded_provider_cannot_clobber_current_status() {
        // Emulate a slow-to-initialize provider (token A) that is superseded by a newer
        // registration (token B) for the same domain before A's terminal event is dispatched.
        let events = EventRegistry::default();

        let count = Arc::new(AtomicUsize::new(0));
        events
            .add_handler(
                None,
                ProviderEventType::Ready,
                Arc::new(counting_handler(&count)),
            )
            .await;

        let token_a = events.on_provider_set("", "provider-a").await;
        let token_b = events.on_provider_set("", "provider-b").await;

        // A's terminal event arrives after it was already replaced: it must be ignored, leaving
        // both the status and the handlers untouched.
        events
            .dispatch(
                "",
                ProviderEventType::Ready,
                &EventDetails::builder().provider_name("provider-a").build(),
                &token_a,
            )
            .await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(events.provider_status("").await, ProviderStatus::NotReady);

        // B is the current provider, so its terminal event is dispatched normally.
        events
            .dispatch(
                "",
                ProviderEventType::Ready,
                &EventDetails::builder().provider_name("provider-b").build(),
                &token_b,
            )
            .await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(events.provider_status("").await, ProviderStatus::Ready);
    }

    /// Read the tracked status from within a synchronous handler.
    ///
    /// Handlers are synchronous, so the async accessor is driven on a separate thread; blocking
    /// on the current runtime from inside it would panic.
    fn read_status_off_runtime(registry: &EventRegistry) -> ProviderStatus {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(registry.provider_status(""))
                })
                .join()
                .unwrap()
        })
    }
}
