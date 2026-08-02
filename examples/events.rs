use std::sync::{Arc, Mutex};

use open_feature::provider::{FeatureProvider, ProviderMetadata, ResolutionDetails};
use open_feature::{
    EvaluationContext, EvaluationResult, EventDetails, EventEmitter, OpenFeature,
    ProviderEventType, StructValue,
};

/// A provider that stores the [`EventEmitter`] handed to it by the SDK and uses it to signal
/// state changes. A real provider would typically emit from a background task watching its
/// flag management system.
struct EventfulProvider {
    metadata: ProviderMetadata,
    emitter: Arc<Mutex<Option<EventEmitter>>>,
}

impl Default for EventfulProvider {
    fn default() -> Self {
        Self {
            metadata: ProviderMetadata::new("Eventful Provider"),
            emitter: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait::async_trait]
impl FeatureProvider for EventfulProvider {
    fn attach_emitter(&mut self, emitter: EventEmitter) {
        *self.emitter.lock().unwrap() = Some(emitter);
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn resolve_bool_value(
        &self,
        _flag_key: &str,
        _evaluation_context: &EvaluationContext,
    ) -> EvaluationResult<ResolutionDetails<bool>> {
        Ok(ResolutionDetails::new(true))
    }

    async fn resolve_int_value(
        &self,
        _flag_key: &str,
        _evaluation_context: &EvaluationContext,
    ) -> EvaluationResult<ResolutionDetails<i64>> {
        Ok(ResolutionDetails::new(42))
    }

    async fn resolve_float_value(
        &self,
        _flag_key: &str,
        _evaluation_context: &EvaluationContext,
    ) -> EvaluationResult<ResolutionDetails<f64>> {
        Ok(ResolutionDetails::new(0.42))
    }

    async fn resolve_string_value(
        &self,
        _flag_key: &str,
        _evaluation_context: &EvaluationContext,
    ) -> EvaluationResult<ResolutionDetails<String>> {
        Ok(ResolutionDetails::new("42".to_string()))
    }

    async fn resolve_struct_value(
        &self,
        _flag_key: &str,
        _evaluation_context: &EvaluationContext,
    ) -> EvaluationResult<ResolutionDetails<StructValue>> {
        Ok(ResolutionDetails::new(StructValue::default()))
    }
}

#[tokio::main]
async fn main() {
    let mut api = OpenFeature::singleton_mut().await;

    // An API-level handler runs for events emitted by any provider.
    // This one is registered before the provider is set, so it observes the
    // `PROVIDER_READY` event dispatched when initialization succeeds.
    api.add_handler(ProviderEventType::Ready, |details| {
        println!("provider '{}' is ready", details.provider_name);
    })
    .await;

    let provider = EventfulProvider::default();
    let emitter = provider.emitter.clone();
    api.set_provider(provider).await.expect("initialization");

    // A client-level handler only runs for events emitted by the provider
    // associated with this client.
    let client = api.create_client();
    client
        .add_handler(ProviderEventType::ConfigurationChanged, |details| {
            println!(
                "provider '{}' changed flags: {:?}",
                details.provider_name, details.flags_changed
            );
        })
        .await;
    drop(api);

    // The provider signals a configuration change; both the message above and
    // the SDK-tracked provider status reflect it.
    let emitter = emitter.lock().unwrap().clone().expect("emitter attached");
    emitter
        .emit(
            ProviderEventType::ConfigurationChanged,
            EventDetails::builder()
                .provider_name("Eventful Provider")
                .flags_changed(vec!["v2_enabled".to_string()])
                .build(),
        )
        .await;

    println!(
        "provider status: {:?}",
        OpenFeature::singleton().await.provider_status().await
    );
}
