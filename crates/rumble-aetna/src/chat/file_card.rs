use aetna_core::prelude::*;
use rumble_client_traits::file_transfer::{TransferDirection, TransferStage, TransferStatus};
use rumble_protocol::proto::RelayFileSharePayload;

use crate::theme as palette;

use super::{
    cancel_key, download_key, file_card_key, format_size, open_key, reveal_key,
    KEY_FILE_CTX_OPEN, KEY_FILE_CTX_OPEN_FOLDER, KEY_FILE_CTX_SAVE_AS,
};

/// State for the right-click file context menu.
#[derive(Clone, Debug)]
pub struct FileContextMenu {
    pub transfer_id: String,
    pub name: String,
    /// Local path if the file has been downloaded; `None` while pending.
    pub local_path: Option<std::path::PathBuf>,
    /// Screen-space anchor for the context menu popover.
    pub point: (f32, f32),
}

const KEY_FILE_CTX: &str = "chat:file_ctx";

/// Render the file card context menu as a floating popover overlay.
pub fn render_file_context_menu(menu: &FileContextMenu) -> El {
    let has_file = menu.local_path.is_some();

    let mut save_as = menu_item("Save As…").key(KEY_FILE_CTX_SAVE_AS);
    let mut open_folder = menu_item("Open Containing Folder").key(KEY_FILE_CTX_OPEN_FOLDER);
    let mut open = menu_item("Open").key(KEY_FILE_CTX_OPEN);
    if !has_file {
        save_as = save_as.disabled();
        open_folder = open_folder.disabled();
        open = open.disabled();
    }

    context_menu(
        KEY_FILE_CTX,
        menu.point,
        [
            text(menu.name.clone())
                .semibold()
                .ellipsis()
                .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
            divider(),
            open,
            open_folder,
            save_as,
        ],
    )
}

pub(super) fn file_offer_card(
    offer: &RelayFileSharePayload,
    status: Option<&TransferStatus>,
    is_local_sender: bool,
    pending_cancel_confirm: &std::collections::HashMap<String, std::time::Instant>,
) -> El {
    let header = file_card_header(offer);
    let meta = file_card_meta(offer);
    let body = file_card_body(offer, status, is_local_sender, pending_cancel_confirm);

    column([header, meta, body])
        .key(file_card_key(&offer.transfer_id))
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// Header row: file-type icon + the filename, ellipsis-truncated with
/// the full name in a tooltip. Shared by the regular card and the failed card.
fn file_card_header(offer: &RelayFileSharePayload) -> El {
    row([
        icon(IconName::FileText).text_color(tokens::MUTED_FOREGROUND),
        text(offer.name.clone())
            .semibold()
            .ellipsis()
            .key(format!("chat:file-card:{}.name", offer.transfer_id))
            .tooltip(offer.name.clone()),
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

/// Secondary line: human size + MIME type.
fn file_card_meta(offer: &RelayFileSharePayload) -> El {
    let mime = if offer.mime.is_empty() {
        "unknown".to_string()
    } else {
        offer.mime.clone()
    };
    text(format!("{} · {mime}", format_size(offer.size)))
        .muted()
        .font_size(tokens::TEXT_XS.size)
}

/// Determine the action/progress body for a file card based on the
/// user's relationship to the file (sender vs receiver) and the
/// transfer's lifecycle stage. The Failed stage is handled upstream
/// in `AttachmentView` — by the time we get here, `status.stage` is
/// `Active`, `Paused`, `Done`, or `status` is `None`.
fn file_card_body(
    offer: &RelayFileSharePayload,
    status: Option<&TransferStatus>,
    is_local_sender: bool,
    pending_cancel_confirm: &std::collections::HashMap<String, std::time::Instant>,
) -> El {
    match status.map(|s| &s.stage) {
        Some(TransferStage::Active { .. } | TransferStage::Paused { .. }) => {
            let s = status.expect("matched on status above");
            let confirm_pending = pending_cancel_confirm.contains_key(&s.id.0);
            transfer_progress_block(s, confirm_pending)
        }
        Some(TransferStage::Done { .. }) if is_local_sender => sender_complete_row(&offer.transfer_id),
        Some(TransferStage::Done { .. }) => receiver_complete_row(offer, status.expect("matched on status above")),
        // No status: sender side renders nothing actionable (their
        // local-only card is just the header). Receiver side gets a
        // Download button to start the fetch.
        None if is_local_sender => row([spacer()]).width(Size::Fill(1.0)),
        None => row([
            spacer(),
            button_with_icon(IconName::Download, "Download")
                .key(download_key(&offer.transfer_id))
                .primary(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
        // Failed is lifted to the AttachmentView::Failed dispatch arm
        // and never reaches the file_card_body; emit a no-op as a
        // defensive default rather than asserting unreachable.
        Some(TransferStage::Failed { .. }) => row([spacer()]).width(Size::Fill(1.0)),
    }
}

/// Failure card body. Replaces the file card entirely when an upload
/// or download fails — the renderer reaches this path via
/// `AttachmentView::Failed` regardless of any preview cache hit.
pub(super) fn failed_card(offer: &RelayFileSharePayload, reason: &str, is_local_sender: bool) -> El {
    let label = if is_local_sender {
        "Upload failed"
    } else {
        "Download failed"
    };
    let header = row([text(label)
        .text_color(palette::MUTED_SERVER)
        .font_size(tokens::TEXT_XS.size)
        .semibold()
        .width(Size::Fill(1.0))])
    .width(Size::Fill(1.0));
    let reason_el = paragraph(reason.to_string())
        .text_color(palette::MUTED_SERVER)
        .font_size(tokens::TEXT_XS.size)
        .key(format!("chat:file-card:{}.err", offer.transfer_id))
        .selectable()
        .width(Size::Fill(1.0));

    let card_header = file_card_header(offer);
    let meta = file_card_meta(offer);
    let mut body: Vec<El> = vec![card_header, meta, header, reason_el];
    if is_local_sender {
        body.push(
            text("Retry by sharing the file again")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        );
    }

    column(body)
        .key(file_card_key(&offer.transfer_id))
        .gap(tokens::SPACE_1)
        .padding(Sides::all(tokens::SPACE_2))
        .fill(tokens::SECONDARY)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
}

/// Open + Reveal buttons for a sender whose upload completed.
fn sender_complete_row(transfer_id: &str) -> El {
    row([
        spacer(),
        button_with_icon(IconName::Folder, "Reveal")
            .key(reveal_key(transfer_id))
            .ghost(),
        button_with_icon(IconName::FileText, "Open")
            .key(open_key(transfer_id))
            .primary(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
    .align(Align::Center)
}

/// Open + Reveal buttons for a receiver whose download completed.
fn receiver_complete_row(offer: &RelayFileSharePayload, status: &TransferStatus) -> El {
    let is_playable_video = status.done_path().is_some() && crate::video::is_video_name(&offer.name);
    let mut btns: Vec<El> = vec![spacer()];
    if is_playable_video {
        btns.push(
            button_with_icon(IconName::Activity, "Play")
                .key(crate::video::open_video_key(&offer.transfer_id))
                .primary(),
        );
    }
    btns.push(
        button_with_icon(IconName::Folder, "Reveal")
            .key(reveal_key(&offer.transfer_id))
            .ghost(),
    );
    btns.push(
        button_with_icon(IconName::FileText, "Open")
            .key(open_key(&offer.transfer_id))
            .primary(),
    );
    row(btns)
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center)
}

/// Progress bar + bytes-transferred + speed/ETA line for an in-flight transfer.
/// `confirm_pending` is true when the first cancel click has been received
/// and the button should show "Cancel?" to prompt confirmation.
fn transfer_progress_block(status: &TransferStatus, confirm_pending: bool) -> El {
    let progress_frac = status.stage.progress();
    let (speed_bps, paused) = match status.stage {
        TransferStage::Active { speed_bps, .. } => (speed_bps, false),
        TransferStage::Paused { .. } => (0, true),
        _ => (0, false),
    };

    let bar: El = if status.size == 0 {
        progress(1.0, tokens::PRIMARY)
    } else {
        progress(progress_frac, tokens::PRIMARY)
    };

    let direction_label = match status.direction {
        TransferDirection::Upload => "Uploading",
        TransferDirection::Download => "Downloading",
    };

    let info_text: El = if status.size == 0 {
        text(format!("{direction_label} · 0 B (empty file)"))
            .muted()
            .font_size(tokens::TEXT_XS.size)
    } else {
        let pct = (progress_frac * 100.0).round() as i32;
        let transferred = (status.size as f32 * progress_frac) as u64;
        let bytes_label = format!("{} / {}", format_size(transferred), format_size(status.size));
        let speed_label = if paused {
            "Paused".to_string()
        } else if speed_bps > 0 {
            format!("{}/s", format_size(speed_bps))
        } else {
            "…".to_string()
        };
        let eta_label = if !paused && speed_bps > 0 && status.size > transferred {
            let remaining = status.size - transferred;
            format!(" · {} left", format_eta(remaining, speed_bps))
        } else {
            String::new()
        };

        text(format!(
            "{direction_label} · {pct}% · {bytes_label} · {speed_label}{eta_label}"
        ))
        .muted()
        .font_size(tokens::TEXT_XS.size)
    };

    // Two-click cancel: first click shows "Cancel?" in red; second fires the command.
    let cancel_btn: El = if confirm_pending {
        button("Cancel?")
            .key(cancel_key(&status.id.0))
            .destructive()
            .font_size(tokens::TEXT_XS.size)
    } else {
        icon_button(IconName::X)
            .key(cancel_key(&status.id.0))
            .ghost()
            .tooltip("Cancel")
    };
    let info_line = row([info_text.width(Size::Fill(1.0)), cancel_btn])
        .gap(tokens::SPACE_1)
        .align(Align::Center)
        .width(Size::Fill(1.0));

    column([bar, info_line]).gap(tokens::SPACE_1).width(Size::Fill(1.0))
}

/// Coarse mm:ss / Hh Mm formatter for a remaining-time estimate.
/// Caller guarantees `speed > 0`.
fn format_eta(remaining_bytes: u64, speed_bps: u64) -> String {
    let secs = remaining_bytes / speed_bps.max(1);
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
