//! What a thread needs from Codex, as traits so a test can stand in for Codex itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_core::CodexThread;
use codex_login::auth::AuthManager;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::Op;
use nori_acp_kit::acp::Error;

/// Trait for abstracting over the `CodexThread` to make testing easier.
pub trait CodexThreadImpl: Send + Sync {
    fn submit(&self, op: Op)
    -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>>;
    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>>;
}

impl CodexThreadImpl for CodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(self.submit(op))
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(self.next_event())
    }
}

pub trait ModelsManagerImpl: Send + Sync {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>>;
}

/// The live models manager, with what it needs to reach the model endpoint.
pub struct LiveModels {
    manager: Arc<dyn ModelsManager>,
    http_client_factory: codex_http_client::HttpClientFactory,
}

impl LiveModels {
    pub fn new(
        manager: Arc<dyn ModelsManager>,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Self {
        Self {
            manager,
            http_client_factory,
        }
    }
}

impl ModelsManagerImpl for LiveModels {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        let model_id = model_id.clone();
        Box::pin(async move {
            self.manager
                .get_default_model(
                    &model_id,
                    /*allow_provider_model_fallback*/ true,
                    RefreshStrategy::OnlineIfUncached,
                    self.http_client_factory.clone(),
                )
                .await
        })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async move {
            self.manager
                .list_models(
                    RefreshStrategy::OnlineIfUncached,
                    self.http_client_factory.clone(),
                )
                .await
        })
    }
}

pub trait Auth {
    fn logout(&self) -> impl Future<Output = Result<bool, Error>> + Send;
}

impl Auth for Arc<AuthManager> {
    async fn logout(&self) -> Result<bool, Error> {
        self.as_ref()
            .logout()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }
}
