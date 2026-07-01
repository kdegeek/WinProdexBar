use std::collections::HashMap;

use crate::core::{
    FetchContext, ProviderAccountData, ProviderId, SourceMode, TokenAccountOverride,
    TokenAccountStore,
};
use crate::settings::{ApiKeys, ManualCookies, Settings};

#[derive(Debug, Clone)]
pub(crate) struct PersistedProviderContexts {
    settings: Settings,
    manual_cookies: ManualCookies,
    api_keys: ApiKeys,
    token_accounts: HashMap<ProviderId, ProviderAccountData>,
}

impl PersistedProviderContexts {
    pub(crate) fn load() -> Self {
        Self {
            settings: Settings::load(),
            manual_cookies: ManualCookies::load(),
            api_keys: ApiKeys::load(),
            token_accounts: TokenAccountStore::new().load().unwrap_or_default(),
        }
    }

    #[cfg(test)]
    fn from_parts(
        settings: Settings,
        manual_cookies: ManualCookies,
        api_keys: ApiKeys,
        token_accounts: HashMap<ProviderId, ProviderAccountData>,
    ) -> Self {
        Self {
            settings,
            manual_cookies,
            api_keys,
            token_accounts,
        }
    }

    pub(crate) fn fetch_context(
        &self,
        id: ProviderId,
        base: &FetchContext,
        source_override: Option<SourceMode>,
    ) -> FetchContext {
        let cookie_source = self.settings.cookie_source(id);
        let stored_cookie = self
            .manual_cookies
            .get(id.cli_name())
            .map(ToString::to_string);
        let stored_api_key = self.api_keys.get(id.cli_name()).map(ToString::to_string);
        let token_override = self
            .token_accounts
            .get(&id)
            .and_then(|data| data.active_account())
            .cloned()
            .map(|account| TokenAccountOverride::from_account(id, account));
        let active_token_cookie = token_override
            .as_ref()
            .and_then(|override_data| override_data.cookie_header.clone());
        let active_token_env = token_override
            .as_ref()
            .and_then(|override_data| override_data.env_override.as_ref());
        let active_token_api_key = active_token_env.and_then(|env| env.values().next().cloned());
        let usage_source = source_override
            .unwrap_or_else(|| SourceMode::parse(self.settings.usage_source(id)).unwrap_or_default());
        let api_key = stored_api_key.or(active_token_api_key);
        let has_kimi_code_api_key =
            id == ProviderId::Kimi && api_key.as_deref().is_some_and(|key| !key.trim().is_empty());

        let (source_mode, cookie_header) = if id.cookie_domain().is_none() {
            let source_mode = if active_token_env.is_some() {
                SourceMode::OAuth
            } else {
                usage_source
            };
            (source_mode, None)
        } else {
            match cookie_source {
                _ if active_token_env.is_some() => (SourceMode::OAuth, None),
                "off" if id == ProviderId::Claude && usage_source != SourceMode::Cli => {
                    (SourceMode::OAuth, None)
                }
                "off" if has_kimi_code_api_key && usage_source == SourceMode::Auto => {
                    (SourceMode::Auto, None)
                }
                "off" => (SourceMode::Cli, None),
                "manual" => {
                    let cookie_header = active_token_cookie.or(stored_cookie);
                    let source_mode = if has_kimi_code_api_key && usage_source == SourceMode::Auto {
                        SourceMode::Auto
                    } else if cookie_header.is_some() {
                        SourceMode::Web
                    } else if id == ProviderId::Claude && usage_source != SourceMode::Cli {
                        SourceMode::OAuth
                    } else if !crate::core::instantiate_provider(id).supports_cli() {
                        SourceMode::Auto
                    } else {
                        SourceMode::Cli
                    };
                    (source_mode, cookie_header)
                }
                "auto" | "browser" | "web" => {
                    let cookie_header = active_token_cookie.or(stored_cookie).or_else(|| {
                        provider_cookie_domain(id, &self.settings).and_then(|domain| {
                            crate::browser::cookies::get_cookie_header(domain)
                                .ok()
                                .filter(|header| !header.is_empty())
                        })
                    });
                    (usage_source, cookie_header)
                }
                _ => (usage_source, stored_cookie),
            }
        };

        let workspace_id = self.settings.workspace_id(id).trim().to_string();
        let api_region = self.settings.api_region(id).trim().to_string();

        FetchContext {
            source_mode,
            manual_cookie_header: cookie_header,
            api_key,
            workspace_id: (!workspace_id.is_empty()).then_some(workspace_id),
            api_region: (!api_region.is_empty()).then_some(api_region),
            ..base.clone()
        }
    }
}

fn provider_cookie_domain(id: ProviderId, settings: &Settings) -> Option<&'static str> {
    if id == ProviderId::MiniMax {
        return Some(crate::providers::MiniMaxProvider::cookie_domain_for_region(Some(
            settings.api_region(id),
        )));
    }
    if id == ProviderId::Alibaba {
        return Some(crate::providers::AlibabaProvider::cookie_domain_for_region(Some(
            settings.api_region(id),
        )));
    }
    id.cookie_domain()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::TokenAccount;

    fn contexts_with(
        settings: Settings,
        manual_cookies: ManualCookies,
        api_keys: ApiKeys,
        token_accounts: HashMap<ProviderId, ProviderAccountData>,
    ) -> PersistedProviderContexts {
        PersistedProviderContexts::from_parts(settings, manual_cookies, api_keys, token_accounts)
    }

    #[test]
    fn manual_cookie_uses_web_source_for_cookie_provider() {
        let settings = Settings::default();
        let mut cookies = ManualCookies::default();
        cookies.set("ollama", "__Secure-session=abc");
        let contexts = contexts_with(settings, cookies, ApiKeys::default(), HashMap::new());

        let ctx = contexts.fetch_context(ProviderId::Ollama, &FetchContext::default(), None);

        assert_eq!(ctx.source_mode, SourceMode::Web);
        assert_eq!(ctx.manual_cookie_header.as_deref(), Some("__Secure-session=abc"));
    }

    #[test]
    fn api_key_is_loaded_for_token_provider() {
        let settings = Settings::default();
        let mut api_keys = ApiKeys::default();
        api_keys.set("deepseek", "sk-test", None);
        let contexts = contexts_with(
            settings,
            ManualCookies::default(),
            api_keys,
            HashMap::new(),
        );

        let ctx = contexts.fetch_context(ProviderId::DeepSeek, &FetchContext::default(), None);

        assert_eq!(ctx.source_mode, SourceMode::Auto);
        assert_eq!(ctx.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn token_account_takes_precedence_over_manual_cookie() {
        let settings = Settings::default();
        let mut cookies = ManualCookies::default();
        cookies.set("ollama", "__Secure-session=old");
        let mut account_data = ProviderAccountData::new();
        account_data.add_account(TokenAccount::new("Cloud", "new"));
        let mut token_accounts = HashMap::new();
        token_accounts.insert(ProviderId::Ollama, account_data);
        let contexts = contexts_with(settings, cookies, ApiKeys::default(), token_accounts);

        let ctx = contexts.fetch_context(ProviderId::Ollama, &FetchContext::default(), None);

        assert_eq!(ctx.source_mode, SourceMode::Web);
        assert_eq!(
            ctx.manual_cookie_header.as_deref(),
            Some("__Secure-session=new")
        );
    }

    #[test]
    fn manual_cookie_default_does_not_force_cli_for_non_cli_provider() {
        let contexts = contexts_with(
            Settings::default(),
            ManualCookies::default(),
            ApiKeys::default(),
            HashMap::new(),
        );

        let ctx = contexts.fetch_context(ProviderId::Ollama, &FetchContext::default(), None);

        assert_eq!(ctx.source_mode, SourceMode::Auto);
        assert!(ctx.manual_cookie_header.is_none());
    }
}
