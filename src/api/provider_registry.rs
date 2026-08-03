use std::sync::Arc;
use std::{borrow::Borrow, collections::HashMap};

use tokio::sync::RwLock;

use crate::provider::{FeatureProvider, NoOpProvider};
use crate::{EvaluationError, EventDetails, EventEmitter, ProviderEventType};

use super::event_registry::EventRegistry;
use super::global_evaluation_context::GlobalEvaluationContext;

// ============================================================
//  ProviderRegistry
// ============================================================

#[derive(Clone)]
pub struct ProviderRegistry {
    global_evaluation_context: GlobalEvaluationContext,
    events: EventRegistry,
    providers: Arc<RwLock<HashMap<String, FeatureProviderWrapper>>>,
}

impl ProviderRegistry {
    pub fn new(evaluation_context: GlobalEvaluationContext, events: EventRegistry) -> Self {
        let mut providers: HashMap<String, FeatureProviderWrapper> = HashMap::new();
        providers.insert(
            String::default(),
            FeatureProviderWrapper::new(NoOpProvider::default()),
        );

        Self {
            global_evaluation_context: evaluation_context,
            events,
            providers: Arc::new(RwLock::new(providers)),
        }
    }

    pub async fn set_default<T: FeatureProvider>(
        &self,
        provider: T,
    ) -> Result<(), EvaluationError> {
        self.set("", provider).await
    }

    pub async fn set_named<T: FeatureProvider>(
        &self,
        name: &str,
        provider: T,
    ) -> Result<(), EvaluationError> {
        self.set(name, provider).await
    }

    /// Register `provider` for the given `domain` (`""` meaning the default provider), replacing
    /// (and thereby dropping) the previously registered one, if any.
    ///
    /// The provider is initialized before it is made available for flag resolution; the
    /// registered `PROVIDER_READY` or `PROVIDER_ERROR` event handlers run depending on the
    /// initialization outcome. The provider stays registered even if initialization fails.
    async fn set<T: FeatureProvider>(
        &self,
        domain: &str,
        mut provider: T,
    ) -> Result<(), EvaluationError> {
        let provider_name = provider.metadata().name.clone();

        // Deactivate the emitter of the previously registered provider and hand a fresh one to
        // the new provider before it is initialized and frozen behind an `Arc`. The emitter
        // stays disarmed until the SDK has dispatched its own terminal readiness event below, so
        // events the provider emits from within `initialize` are ignored (the SDK derives the
        // initial `PROVIDER_READY`/`PROVIDER_ERROR` event from the initialization outcome).
        let token = self.events.on_provider_set(domain, &provider_name).await;
        let emitter = EventEmitter::new(domain.to_string(), self.events.clone(), token.clone());
        provider.attach_emitter(emitter.clone());

        let result = provider
            .initialize(self.global_evaluation_context.get().await.borrow())
            .await;

        self.providers
            .write()
            .await
            .insert(domain.to_string(), FeatureProviderWrapper::new(provider));

        match &result {
            Ok(()) => {
                let details = EventDetails::builder().provider_name(provider_name).build();
                self.events
                    .dispatch(domain, ProviderEventType::Ready, &details, &token)
                    .await;
            }
            Err(error) => {
                let mut details = EventDetails::builder()
                    .provider_name(provider_name)
                    .error_code(error.code.clone())
                    .build();
                details.message.clone_from(&error.message);
                self.events
                    .dispatch(domain, ProviderEventType::Error, &details, &token)
                    .await;
            }
        }

        // Arm the emitter so events the provider emits from now on are delivered. If this
        // provider was already superseded during initialization, its token is inactive and the
        // emitter remains an effective no-op.
        emitter.arm();

        result
    }

    pub async fn get(&self, name: &str) -> FeatureProviderWrapper {
        match self.get_named(name).await {
            Some(provider) => provider,
            None => self.get_default().await,
        }
    }

    pub async fn get_default(&self) -> FeatureProviderWrapper {
        self.providers.read().await.get("").unwrap().clone()
    }

    pub async fn get_named(&self, name: &str) -> Option<FeatureProviderWrapper> {
        self.providers.read().await.get(name).cloned()
    }

    pub async fn clear(&self) {
        self.providers.write().await.clear();
        self.events.clear().await;
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new(GlobalEvaluationContext::default(), EventRegistry::default())
    }
}

// ============================================================
//  FeatureProviderWrapper
// ============================================================

#[derive(Clone)]
pub struct FeatureProviderWrapper(Arc<dyn FeatureProvider>);

impl FeatureProviderWrapper {
    pub fn new(provider: impl FeatureProvider) -> Self {
        Self(Arc::new(provider))
    }

    pub fn get(&self) -> Arc<dyn FeatureProvider> {
        self.0.clone()
    }
}
