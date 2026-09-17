use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HomeSyncRoute {
    OnetCloud,
    Personal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HomeSyncButtonState {
    Ready,
    NeedsSettings,
    Disabled,
}

impl HomeSyncButtonState {
    pub(super) fn is_disabled(self) -> bool {
        self == Self::Disabled
    }
}

pub(super) struct HomeSyncButtonContext {
    pub route: HomeSyncRoute,
    pub sync_enabled: bool,
    pub is_logged_in: bool,
    pub has_sync_license: bool,
    pub onet_syncing: bool,
    pub personal_sync_ready: bool,
    pub personal_syncing: bool,
}

pub(super) fn home_sync_button_state(context: HomeSyncButtonContext) -> HomeSyncButtonState {
    let syncing = match context.route {
        HomeSyncRoute::OnetCloud => context.onet_syncing,
        HomeSyncRoute::Personal => context.personal_syncing,
    };
    if syncing {
        return HomeSyncButtonState::Disabled;
    }
    if !context.sync_enabled {
        return HomeSyncButtonState::NeedsSettings;
    }

    match context.route {
        HomeSyncRoute::OnetCloud if !context.is_logged_in && context.has_sync_license => {
            HomeSyncButtonState::Disabled
        }
        HomeSyncRoute::Personal if !context.personal_sync_ready => HomeSyncButtonState::Disabled,
        _ => HomeSyncButtonState::Ready,
    }
}

pub(super) fn refreshed_pending_conflicts(
    previous: Vec<SyncConflict>,
    current: Vec<SyncConflict>,
    errors: &[String],
) -> Vec<SyncConflict> {
    if errors.is_empty() || !current.is_empty() {
        current
    } else {
        previous
    }
}

pub(super) fn sync_route_for_provider(provider: SyncProvider) -> HomeSyncRoute {
    match provider {
        SyncProvider::OnetCloud => HomeSyncRoute::OnetCloud,
        SyncProvider::Personal => HomeSyncRoute::Personal,
    }
}

pub(super) fn sync_route(cx: &App) -> HomeSyncRoute {
    sync_route_for_provider(AppSettings::global(cx).sync_provider)
}

pub(super) fn should_auto_onet_cloud_sync_for_settings(
    _settings: &AppSettings,
    _current_user_present: bool,
    _has_master_key: bool,
) -> bool {
    false
}

pub(super) fn should_auto_onet_cloud_sync(cx: &App, current_user_present: bool) -> bool {
    should_auto_onet_cloud_sync_for_settings(
        AppSettings::global(cx),
        current_user_present,
        crypto::has_master_key(),
    )
}

pub(super) fn should_show_team_key_menu_item(
    route: HomeSyncRoute,
    cached_team_count: usize,
) -> bool {
    route == HomeSyncRoute::OnetCloud && cached_team_count > 0
}

pub(crate) fn should_show_team_management_entry(_team_management_enabled: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_cloud_sync_never_runs_automatically() {
        let mut settings = AppSettings::default();
        settings.sync_provider = SyncProvider::OnetCloud;
        settings.sync_enabled = false;

        assert!(!should_auto_onet_cloud_sync_for_settings(
            &settings, true, true
        ));

        settings.sync_enabled = true;
        assert!(!should_auto_onet_cloud_sync_for_settings(
            &settings, true, true
        ));
    }

    #[test]
    fn onet_cloud_auto_sync_still_requires_provider_user_and_master_key() {
        let mut settings = AppSettings::default();
        settings.sync_enabled = true;
        settings.sync_provider = SyncProvider::Personal;

        assert!(!should_auto_onet_cloud_sync_for_settings(
            &settings, true, true
        ));

        settings.sync_provider = SyncProvider::OnetCloud;
        assert!(!should_auto_onet_cloud_sync_for_settings(
            &settings, false, true
        ));
        assert!(!should_auto_onet_cloud_sync_for_settings(
            &settings, true, false
        ));
    }
}
