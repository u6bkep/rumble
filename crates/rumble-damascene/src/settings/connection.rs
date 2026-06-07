//! Connection tab: identity detail, auto-connect, superuser elevation.

use super::*;

pub(super) fn render_connection(pending: &PendingSettings, identity: &Identity, app_state: &State) -> El {
    use rumble_desktop_shell::KeySource;

    let mut cards: Vec<El> = Vec::new();

    let Some(config) = identity.manager().config().cloned() else {
        cards.push(section_card(
            "Identity",
            [
                paragraph("No identity configured.").text_color(tokens::WARNING),
                row([spacer(), button("Set up identity…").key(KEY_REGENERATE).primary()]).width(Size::Fill(1.0)),
            ],
        ));
        cards.push(section_card("Auto-connect", render_autoconnect_rows(pending)));
        if let Some(superuser) = render_superuser_section(app_state) {
            cards.push(section_card("Superuser", superuser));
        }
        return column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0));
    };

    // ---- Source detail ----
    let (storage_label, storage_detail) = match &config.source {
        KeySource::LocalPlaintext { .. } => (
            "Local (unencrypted)",
            "Stored unencrypted at identity.json — fine for personal machines.".to_string(),
        ),
        KeySource::LocalEncrypted { .. } => (
            "Local (password protected)",
            "Encrypted with Argon2 + ChaCha20-Poly1305. Password required at startup.".to_string(),
        ),
        KeySource::SshAgent {
            fingerprint: agent_fp,
            comment,
        } => {
            let detail = if comment.is_empty() {
                format!("Bound to ssh-agent key {agent_fp}.")
            } else {
                format!("Bound to ssh-agent key \"{comment}\" ({agent_fp}).")
            };
            ("SSH agent", detail)
        }
    };

    let mut identity_rows: Vec<El> = Vec::new();
    identity_rows.push(field_row("Storage", text(storage_label.to_string()).semibold()));
    identity_rows.push(paragraph(storage_detail).muted().font_size(tokens::TEXT_XS.size));

    // For ssh-agent identities: show whether the agent socket is
    // reachable right now. We can't verify the bound key is loaded
    // without an async call, but env-level reachability is the most
    // common failure mode (running Rumble outside a desktop session,
    // forgot to start the agent, etc.).
    let agent_reachable = ssh_agent_available();
    if matches!(config.source, KeySource::SshAgent { .. }) {
        if agent_reachable {
            identity_rows.push(
                alert([
                    alert_title("SSH agent reachable"),
                    alert_description(
                        "Rumble will ask the agent to sign on connect. The agent must still hold this key.",
                    ),
                ])
                .info(),
            );
        } else {
            identity_rows.push(
                alert([
                    alert_title("SSH agent not reachable"),
                    alert_description(
                        "SSH_AUTH_SOCK is unset in this environment. Signing will fail when you try to connect — \
                         start ssh-agent (or relaunch Rumble from a shell with SSH_AUTH_SOCK set).",
                    ),
                ])
                .warning(),
            );
        }
    }

    identity_rows.push(field_row(
        "Fingerprint",
        mono(identity.fingerprint())
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));
    if let Some(pubkey) = identity.public_key() {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pubkey);
        identity_rows.push(field_row(
            "Public key",
            row([
                mono(b64)
                    .font_size(tokens::TEXT_XS.size)
                    .ellipsis()
                    .width(Size::Fill(1.0)),
                button("Copy").key(KEY_COPY_PUBKEY),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
        ));
    }
    let path = identity.manager().config_dir().join("identity.json");
    identity_rows.push(field_row(
        "On disk",
        mono(path.display().to_string())
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));

    // ---- Switch source (non-destructive) ----
    // For local sources: "Switch to SSH agent key…".
    // For agent sources: "Re-select SSH agent key…" (re-pick from agent;
    // the local identity file just gets rewritten with the new
    // fingerprint, no key material is destroyed).
    let agent_btn_label = match &config.source {
        KeySource::SshAgent { .. } => "Re-select SSH agent key…",
        _ => "Switch to SSH agent key…",
    };
    let agent_btn = button(agent_btn_label).key(KEY_SWITCH_AGENT).secondary();
    let agent_btn = if agent_reachable {
        agent_btn
    } else {
        agent_btn.disabled()
    };
    identity_rows.push(row([spacer(), agent_btn]).width(Size::Fill(1.0)));

    cards.push(section_card("Identity", identity_rows));

    cards.push(section_card(
        "Replace identity",
        [
            alert([
                alert_title("Regenerating overwrites your identity"),
                alert_description(
                    "Servers that knew the old key won't recognise the new one — you'll have to re-register or be \
                     re-approved.",
                ),
            ])
            .warning(),
            row([
                spacer(),
                button("Generate new identity…").key(KEY_REGENERATE).destructive(),
            ])
            .width(Size::Fill(1.0)),
        ],
    ));

    cards.push(section_card("Auto-connect", render_autoconnect_rows(pending)));

    if let Some(superuser) = render_superuser_section(app_state) {
        cards.push(section_card("Superuser", superuser));
    }

    column(cards).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

fn render_autoconnect_rows(pending: &PendingSettings) -> Vec<El> {
    vec![
        field_row(
            "Autoconnect on launch",
            switch(pending.autoconnect).key(KEY_AUTOCONNECT),
        ),
        paragraph(
            "Reconnects to the most recently used server on startup. Effective once you've connected to at least one \
             server.",
        )
        .muted()
        .font_size(tokens::TEXT_XS.size),
    ]
}

/// True when the ssh-agent socket appears reachable from this process.
/// Mirrors wizard.rs's check; sync because it's a bare env-var test.
fn ssh_agent_available() -> bool {
    rumble_desktop_shell::identity::SshAgentClient::is_available()
}

/// Render the Elevate / superuser section of the Connection tab, or
/// `None` when the local user can't sudo (not connected, or the
/// server's effective permissions don't carry [`Permissions::SUDO`]).
/// When already elevated, swaps the button for a status notice.
fn render_superuser_section(app_state: &State) -> Option<Vec<El>> {
    if !app_state.connection.is_connected() {
        return None;
    }
    let perms = Permissions::from_bits_truncate(app_state.effective_permissions);
    if !perms.contains(Permissions::SUDO) {
        return None;
    }
    let already_elevated = app_state
        .my_user_id
        .and_then(|id| app_state.get_user(id))
        .map(|u| u.is_elevated)
        .unwrap_or(false);

    let mut out: Vec<El> = Vec::new();
    if already_elevated {
        out.push(
            alert([
                alert_title("Elevated this session"),
                alert_description("You're operating as superuser. Disconnect to drop the elevation."),
            ])
            .info(),
        );
    } else {
        out.push(
            paragraph(
                "Elevation bypasses the ACL system for the rest of this session — disconnect to drop it. Requires the \
                 server-side sudo password set via `server set-sudo-password`.",
            )
            .muted()
            .font_size(tokens::TEXT_XS.size),
        );
        out.push(row([spacer(), button("Elevate to superuser…").key(KEY_ELEVATE).primary()]).width(Size::Fill(1.0)));
    }
    Some(out)
}
