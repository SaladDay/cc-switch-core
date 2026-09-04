//! Single built-in application integration catalog.

use crate::{
    native_import::NativeImportBehavior, projection::NativeProjectionBehavior,
    registry::AppDescriptor, simple_provider::SimpleProviderBehavior, AppType, LogicalTarget,
    SimpleProviderFormDescriptor,
};

/// All contracts that must be registered for one built-in application.
#[derive(Debug)]
pub(crate) struct AppIntegration {
    descriptor: AppDescriptor,
    targets: &'static [LogicalTarget],
    simple_provider_form: &'static SimpleProviderFormDescriptor,
    simple_provider_behavior: SimpleProviderBehavior,
    native_import_behavior: NativeImportBehavior,
    native_projection_behavior: NativeProjectionBehavior,
}

impl AppIntegration {
    pub(crate) const fn new(
        descriptor: AppDescriptor,
        targets: &'static [LogicalTarget],
        simple_provider_form: &'static SimpleProviderFormDescriptor,
        simple_provider_behavior: SimpleProviderBehavior,
        native_import_behavior: NativeImportBehavior,
        native_projection_behavior: NativeProjectionBehavior,
    ) -> Self {
        Self {
            descriptor,
            targets,
            simple_provider_form,
            simple_provider_behavior,
            native_import_behavior,
            native_projection_behavior,
        }
    }

    pub(crate) const fn descriptor(&self) -> &AppDescriptor {
        &self.descriptor
    }

    pub(crate) const fn targets(&self) -> &'static [LogicalTarget] {
        self.targets
    }

    pub(crate) const fn simple_provider_form(&self) -> &SimpleProviderFormDescriptor {
        self.simple_provider_form
    }

    pub(crate) const fn simple_provider_behavior(&self) -> SimpleProviderBehavior {
        self.simple_provider_behavior
    }

    pub(crate) const fn native_import_behavior(&self) -> NativeImportBehavior {
        self.native_import_behavior
    }

    pub(crate) const fn native_projection_behavior(&self) -> NativeProjectionBehavior {
        self.native_projection_behavior
    }
}

static BUILTIN_APP_INTEGRATIONS: [AppIntegration; 9] = [
    crate::claude::INTEGRATION,
    crate::claude_desktop::INTEGRATION,
    crate::codex::INTEGRATION,
    crate::gemini::INTEGRATION,
    crate::grokbuild::INTEGRATION,
    crate::opencode::INTEGRATION,
    crate::openclaw::INTEGRATION,
    crate::hermes::INTEGRATION,
    crate::pi::INTEGRATION,
];

pub(crate) fn builtin_app_integrations(
) -> impl ExactSizeIterator<Item = &'static AppIntegration> + DoubleEndedIterator + Clone {
    BUILTIN_APP_INTEGRATIONS.iter()
}

pub(crate) fn builtin_app_integration(app: &AppType) -> &'static AppIntegration {
    let index = match app {
        AppType::Claude => 0,
        AppType::ClaudeDesktop => 1,
        AppType::Codex => 2,
        AppType::Gemini => 3,
        AppType::GrokBuild => 4,
        AppType::OpenCode => 5,
        AppType::OpenClaw => 6,
        AppType::Hermes => 7,
        AppType::Pi => 8,
    };
    &BUILTIN_APP_INTEGRATIONS[index]
}
