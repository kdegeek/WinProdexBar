use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{ProviderFetchResult, ProviderId, RateWindow, UsageSnapshot};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayPayload {
    pub schema_version: u8,
    pub canvas: DisplayCanvas,
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<DisplayProviderRow>,
    pub attention: Option<DisplayAttention>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DisplayCanvas {
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayProviderRow {
    pub provider: String,
    pub title: Option<String>,
    pub pressure_percent: f64,
    pub identity: DisplayProviderIdentity,
    pub windows: Vec<DisplayProviderWindow>,
    pub status: DisplayProviderStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayProviderIdentity {
    pub accent: &'static str,
    pub background: &'static str,
    pub mark: &'static str,
    pub asset_state: &'static str,
    pub treatment: DisplayIdentityTreatment,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DisplayIdentityTreatment {
    Animated,
    StaticMark,
    RainbowStatic,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayProviderWindow {
    pub id: String,
    pub kind: DisplayProviderWindowKind,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DisplayProviderWindowKind {
    Session,
    Weekly,
    Model,
    Tertiary,
    Extra,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DisplayProviderStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayAttention {
    pub provider: String,
    pub reason: String,
    pub action: String,
}

pub struct DisplayPayloadBuilder;

impl DisplayPayloadBuilder {
    pub fn payload<'a>(
        rows: impl IntoIterator<Item = (ProviderId, &'a ProviderFetchResult)>,
    ) -> DisplayPayload {
        Self::payload_at(rows, Utc::now())
    }

    pub fn payload_at<'a>(
        rows: impl IntoIterator<Item = (ProviderId, &'a ProviderFetchResult)>,
        generated_at: DateTime<Utc>,
    ) -> DisplayPayload {
        let mut display_rows = rows
            .into_iter()
            .filter_map(|(provider_id, result)| Self::display_row(provider_id, result))
            .fold(Vec::<DisplayProviderRow>::new(), |mut acc, row| {
                match acc
                    .iter_mut()
                    .find(|candidate| candidate.provider == row.provider)
                {
                    Some(existing) if row.pressure_percent > existing.pressure_percent => {
                        *existing = row;
                    }
                    None => acc.push(row),
                    _ => {}
                }
                acc
            });

        display_rows.sort_by(|a, b| {
            b.pressure_percent
                .total_cmp(&a.pressure_percent)
                .then_with(|| a.provider.cmp(&b.provider))
        });
        display_rows.truncate(4);

        DisplayPayload {
            schema_version: 1,
            canvas: DisplayCanvas {
                width: 240,
                height: 240,
            },
            generated_at,
            providers: display_rows,
            attention: None,
        }
    }

    fn display_row(
        provider_id: ProviderId,
        result: &ProviderFetchResult,
    ) -> Option<DisplayProviderRow> {
        if !Self::is_supported(provider_id) {
            return None;
        }
        let windows = Self::windows_from_usage(&result.usage);
        if windows.is_empty() {
            return None;
        }
        let pressure_percent = windows
            .iter()
            .map(|window| window.used_percent)
            .fold(0.0_f64, f64::max)
            .clamp(0.0, 100.0);

        Some(DisplayProviderRow {
            provider: provider_id.cli_name().to_string(),
            title: None,
            pressure_percent,
            identity: Self::identity(provider_id)?,
            windows,
            status: DisplayProviderStatus::Ok,
        })
    }

    fn is_supported(provider_id: ProviderId) -> bool {
        matches!(
            provider_id,
            ProviderId::Codex | ProviderId::Claude | ProviderId::Ollama | ProviderId::Antigravity
        )
    }

    fn windows_from_usage(usage: &UsageSnapshot) -> Vec<DisplayProviderWindow> {
        let mut windows = vec![Self::window(
            "primary",
            DisplayProviderWindowKind::Session,
            &usage.primary,
        )];
        if let Some(window) = &usage.secondary {
            windows.push(Self::window(
                "secondary",
                DisplayProviderWindowKind::Weekly,
                window,
            ));
        }
        if let Some(window) = &usage.model_specific {
            windows.push(Self::window(
                "model",
                DisplayProviderWindowKind::Model,
                window,
            ));
        }
        if let Some(window) = &usage.tertiary {
            windows.push(Self::window(
                "tertiary",
                DisplayProviderWindowKind::Tertiary,
                window,
            ));
        }
        for extra in &usage.extra_rate_windows {
            windows.push(Self::window(
                &extra.id,
                DisplayProviderWindowKind::Extra,
                &extra.window,
            ));
        }
        windows
    }

    fn window(
        id: impl Into<String>,
        kind: DisplayProviderWindowKind,
        window: &RateWindow,
    ) -> DisplayProviderWindow {
        DisplayProviderWindow {
            id: id.into(),
            kind,
            used_percent: window.used_percent.clamp(0.0, 100.0),
            resets_at: window.resets_at,
        }
    }

    fn identity(provider_id: ProviderId) -> Option<DisplayProviderIdentity> {
        match provider_id {
            ProviderId::Codex => Some(DisplayProviderIdentity {
                accent: "#1f6feb",
                background: "#06152f",
                mark: "codex",
                asset_state: "pet",
                treatment: DisplayIdentityTreatment::Animated,
            }),
            ProviderId::Claude => Some(DisplayProviderIdentity {
                accent: "#ff8c00",
                background: "#2a1200",
                mark: "claude",
                asset_state: "sprite",
                treatment: DisplayIdentityTreatment::Animated,
            }),
            ProviderId::Ollama => Some(DisplayProviderIdentity {
                accent: "#f5f5f5",
                background: "#050505",
                mark: "ollama",
                asset_state: "static",
                treatment: DisplayIdentityTreatment::StaticMark,
            }),
            ProviderId::Antigravity => Some(DisplayProviderIdentity {
                accent: "#ff4fd8",
                background: "#090912",
                mark: "antigravity",
                asset_state: "rainbow",
                treatment: DisplayIdentityTreatment::RainbowStatic,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CostSnapshot, UsageSnapshot};

    #[test]
    fn ranks_supported_providers_by_quota_pressure() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let rows = vec![
            (ProviderId::Codex, result(72.0, Some(12.0), now)),
            (ProviderId::Claude, result(12.0, Some(91.0), now)),
            (ProviderId::OpenAIApi, result(99.0, None, now)),
            (ProviderId::Ollama, result(48.0, Some(40.0), now)),
            (ProviderId::Antigravity, result(30.0, Some(22.0), now)),
        ];

        let payload = DisplayPayloadBuilder::payload_at(
            rows.iter().map(|(provider, result)| (*provider, result)),
            now,
        );

        assert_eq!(payload.schema_version, 1);
        assert_eq!(
            payload.canvas,
            DisplayCanvas {
                width: 240,
                height: 240
            }
        );
        assert_eq!(
            payload
                .providers
                .iter()
                .map(|row| row.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "ollama", "antigravity"]
        );
        assert_eq!(
            payload
                .providers
                .iter()
                .map(|row| row.pressure_percent.round() as u8)
                .collect::<Vec<_>>(),
            vec![91, 72, 48, 30]
        );
    }

    #[test]
    fn carries_color_and_asset_identity_without_provider_titles() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let rows = vec![
            (ProviderId::Codex, result(24.0, Some(12.0), now)),
            (ProviderId::Claude, result(42.0, Some(15.0), now)),
            (ProviderId::Ollama, result(16.0, Some(11.0), now)),
            (ProviderId::Antigravity, result(8.0, Some(6.0), now)),
        ];

        let payload = DisplayPayloadBuilder::payload_at(
            rows.iter().map(|(provider, result)| (*provider, result)),
            now,
        );

        let identity = |provider: &str| {
            payload
                .providers
                .iter()
                .find(|row| row.provider == provider)
                .map(|row| &row.identity)
                .unwrap()
        };

        assert_eq!(identity("codex").accent, "#1f6feb");
        assert_eq!(
            identity("codex").treatment,
            DisplayIdentityTreatment::Animated
        );
        assert_eq!(identity("claude").accent, "#ff8c00");
        assert_eq!(
            identity("claude").treatment,
            DisplayIdentityTreatment::Animated
        );
        assert_eq!(identity("ollama").accent, "#f5f5f5");
        assert_eq!(identity("ollama").background, "#050505");
        assert_eq!(
            identity("antigravity").treatment,
            DisplayIdentityTreatment::RainbowStatic
        );
        assert!(payload.providers.iter().all(|row| row.title.is_none()));
    }

    fn result(primary: f64, secondary: Option<f64>, now: DateTime<Utc>) -> ProviderFetchResult {
        let mut usage = UsageSnapshot::new(RateWindow::new(primary));
        usage.updated_at = now;
        usage.secondary = secondary.map(RateWindow::new);
        ProviderFetchResult {
            usage,
            cost: Option::<CostSnapshot>::None,
            source_label: "test".to_string(),
        }
    }
}
