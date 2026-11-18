//! The `Family` section: Octo Family, accounts for the people an owner shares their music
//! with.

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::family::device::parse_networks;

/// The `MembersPath` value that keeps members' music next to the music folder.
pub const MEMBERS_NEXT_TO_MUSIC: &str = "Auto";

/// The folder inside the music folder that holds members' music when `MembersPath` is `Auto`.
pub const FAMILY_FOLDER: &str = ".octo-family";

/// Octo Family's settings. Off by default, and with `Enabled` false nothing Octo answers
/// changes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct FamilySettings {
    /// Turns Family on. Environment variable: FAMILY__ENABLED (docker-compose maps
    /// FAMILY_ENABLED).
    pub enabled: bool,
    /// Where members' own libraries live, at the same path inside Octo and Navidrome. `Auto`
    /// (the default) is a `.octo-family` folder in the music folder Octo shares with
    /// Navidrome; anything else is a full folder path.
    pub members_path: String,
    /// Networks that count as home (commas or spaces between them; a bare address is one
    /// host). Everything else is away.
    pub home_networks: String,
    /// Proxies listed here may name the client address through `X-Forwarded-For` (the tunnel
    /// and any reverse proxy beside Octo). No other header is read for it.
    pub trusted_proxies: String,
    /// Days an invite link works.
    pub invite_days: u32,
    /// Whether the owner's own apps show members' personal libraries.
    pub owner_sees_member_libraries: bool,
    /// Whether the owner's own apps show members' playlists.
    pub owner_sees_member_playlists: bool,
    /// What Navidrome accounts outside the family get: "AsToday" (unchanged).
    pub unmanaged_accounts: String,
    /// Kid profiles also skip songs with no explicit mark.
    pub kid_unrated_is_explicit: bool,
    /// How many times a capped member's bitrate a fast pull may average before it is slowed.
    pub fast_pull_factor: u32,
    /// Radio streams Octo allows per person in the family.
    pub radio_streams_per_person: u32,
    /// Whether managers may manage the family from away from home.
    pub manage_from_away: bool,
    /// A playlist created by any Subsonic app whose name starts with this prefix becomes a
    /// family playlist named the rest after it (blank, or spaces only, turns the capture
    /// off). Environment variable: FAMILY__PLAYLIST_PREFIX (docker-compose maps
    /// PLAYLIST_PREFIX).
    pub playlist_prefix: String,
}

impl Default for FamilySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            members_path: MEMBERS_NEXT_TO_MUSIC.to_string(),
            home_networks: "127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, ::1/128, fc00::/7"
                .to_string(),
            trusted_proxies: "127.0.0.0/8, ::1/128".to_string(),
            invite_days: 7,
            owner_sees_member_libraries: false,
            owner_sees_member_playlists: false,
            unmanaged_accounts: "AsToday".to_string(),
            kid_unrated_is_explicit: false,
            fast_pull_factor: 8,
            radio_streams_per_person: 2,
            manage_from_away: true,
            playlist_prefix: String::new(),
        }
    }
}

impl FamilySettings {
    /// The home networks, read from `HomeNetworks`; pieces that are not networks are skipped.
    pub fn home_network_list(&self) -> Vec<IpNet> {
        parse_networks(&self.home_networks).0
    }

    /// The trusted proxies, read from `TrustedProxies`; pieces that are not networks are
    /// skipped.
    pub fn trusted_proxy_list(&self) -> Vec<IpNet> {
        parse_networks(&self.trusted_proxies).0
    }

    /// What to tell the owner when requests come through a proxy at `proxy` that is not trusted:
    /// the exact setting that fixes it.
    pub fn untrusted_proxy_fix(&self, proxy: std::net::IpAddr) -> String {
        let listed = self.trusted_proxies.trim().trim_end_matches(',').trim();
        let value = if listed.is_empty() {
            proxy.to_string()
        } else {
            format!("{listed}, {proxy}")
        };
        format!(
            "Octo is reached through a proxy at {proxy} that it does not trust, so everyone counts \
             as away from home. Set Family:TrustedProxies (environment variable \
             FAMILY__TRUSTEDPROXIES) to \"{value}\" and restart Octo."
        )
    }

    /// Whether `MembersPath` asks for the folder next to the music: `Auto`, or blank.
    pub fn members_path_is_auto(&self) -> bool {
        let path = self.members_path.trim();
        path.is_empty() || path.eq_ignore_ascii_case(MEMBERS_NEXT_TO_MUSIC)
    }

    /// The prefix a playlist's name must start with for its creation to become a family
    /// playlist: the setting exactly as written (a trailing space is meaningful), or none
    /// while the setting is blank or spaces only, which turns the capture off.
    pub fn playlist_prefix_as_set(&self) -> Option<&str> {
        let prefix = self.playlist_prefix.as_str();
        (!prefix.trim().is_empty()).then_some(prefix)
    }

    /// The members folder without trailing slashes: `<music_root>/.octo-family` for `Auto`,
    /// else the path as written.
    pub fn members_path_in(&self, music_root: &str) -> String {
        if self.members_path_is_auto() {
            return next_to_music(music_root);
        }
        let path = self.members_path.trim().trim_end_matches('/');
        if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        }
    }
}

/// The members folder next to the music folder `music_root`: `<music_root>/.octo-family`.
pub fn next_to_music(music_root: &str) -> String {
    format!("{}/{FAMILY_FOLDER}", music_root.trim().trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigTree;
    use crate::settings::AppSettings;
    use std::net::IpAddr;

    #[test]
    fn defaults_are_off_and_safe() {
        let s = FamilySettings::default();
        assert!(!s.enabled);
        assert_eq!(s.members_path, "Auto");
        assert!(s.members_path_is_auto());
        assert_eq!(s.members_path_in("/music/"), "/music/.octo-family");
        assert!(!s.owner_sees_member_libraries);
        assert!(!s.owner_sees_member_playlists);
        assert_eq!(s.unmanaged_accounts, "AsToday");
        assert!(!s.kid_unrated_is_explicit);
        assert_eq!(s.invite_days, 7);
        assert_eq!((s.fast_pull_factor, s.radio_streams_per_person), (8, 2));
        assert!(s.manage_from_away);
        // The playlist capture is off until a prefix is set.
        assert_eq!(s.playlist_prefix, "");
        assert!(s.playlist_prefix_as_set().is_none());
        // Only the machine itself is a trusted proxy until the owner says otherwise.
        let trusted = s.trusted_proxy_list();
        assert_eq!(trusted.len(), 2);
        assert!(
            trusted
                .iter()
                .any(|n| n.contains(&"127.0.0.1".parse::<IpAddr>().unwrap()))
        );
        assert!(
            !trusted
                .iter()
                .any(|n| n.contains(&"192.168.1.1".parse::<IpAddr>().unwrap()))
        );
        assert_eq!(s.home_network_list().len(), 6);
        // An empty configuration binds to exactly these defaults.
        let (settings, warnings) = AppSettings::bind(&ConfigTree::new());
        assert!(warnings.is_empty());
        assert_eq!(settings.family, s);
    }

    #[test]
    fn an_untrusted_proxy_is_answered_with_the_setting_that_trusts_it() {
        let s = FamilySettings::default();
        let fix = s.untrusted_proxy_fix("172.18.0.5".parse().unwrap());
        assert!(
            fix.contains("Family:TrustedProxies") && fix.contains("FAMILY__TRUSTEDPROXIES"),
            "{fix}"
        );
        assert!(fix.contains("\"127.0.0.0/8, ::1/128, 172.18.0.5\""), "{fix}");
    }

    #[test]
    fn the_section_binds_from_configuration() {
        let mut tree = ConfigTree::new();
        tree.set("Family:Enabled", Some("true".into()));
        tree.set("Family:MembersPath", Some("/srv/members/".into()));
        tree.set("Family:TrustedProxies", Some("127.0.0.1, 172.18.0.0/16".into()));
        tree.set("Family:PlaylistPrefix", Some("#family".into()));
        let (settings, warnings) = AppSettings::bind(&tree);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(settings.family.enabled);
        assert!(!settings.family.members_path_is_auto());
        assert_eq!(settings.family.members_path_in("/music"), "/srv/members");
        assert_eq!(settings.family.trusted_proxy_list().len(), 2);
        assert_eq!(settings.family.playlist_prefix_as_set(), Some("#family"));
    }

    #[test]
    fn a_blank_playlist_prefix_keeps_the_capture_off() {
        for blank in ["", " ", "\t"] {
            let s = FamilySettings {
                playlist_prefix: blank.into(),
                ..Default::default()
            };
            assert!(s.playlist_prefix_as_set().is_none(), "{blank:?}");
        }
        // The prefix is matched exactly as written, spaces included: a trailing space is a
        // way to keep `#familyRoad` from matching.
        let s = FamilySettings {
            playlist_prefix: "#family ".into(),
            ..Default::default()
        };
        assert_eq!(s.playlist_prefix_as_set(), Some("#family "));
    }

    #[test]
    fn the_playlist_prefix_binds_from_the_environment_in_every_spelling() {
        // The binder matches keys ignoring case and underscores: the documented spelling,
        // and the one docker-compose writes into the container, are the same setting.
        for name in ["FAMILY__PLAYLIST_PREFIX", "Family__PlaylistPrefix"] {
            let tree = crate::config::ConfigTree::from_env_vars([(name, "#family")]);
            let (settings, warnings) = AppSettings::bind(&tree);
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
            assert_eq!(
                settings.family.playlist_prefix_as_set(),
                Some("#family"),
                "{name}"
            );
        }
    }
}
