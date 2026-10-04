//! egui overlay of a session: toolbar, statistics, transfers, USB window.
//! (The launcher is a web page: see `app/launcher.rs` and common/web.)
//! Screens only describe the UI and return [`Action`]s; the app carries them out.

use egui::{Align2, Color32, RichText};
use nya_ui::egui;

use crate::events::Hotkey;
use crate::input;
use crate::session::{Session, TransferState};

pub enum Action {
    Hotkey(Hotkey),
    SetGameMode(bool),
    SelectDisplay(u32),
    /// Show this host display in an extra window.
    OpenWindow(u32),
    /// Add a virtual screen to the host and show it in a new window.
    NewVirtualWindow,
    SetDisplayChoice(crate::session::DisplayChoice),
    SetGrab(bool),
    SetPolicy(nya_proto::pb::BitratePolicy),
    SetMic(bool),
    /// Watching: operate the host (true: disconnect the current operator).
    TakeControl(bool),
    ToggleUsb,
    InstallUsbipd,
    RefreshUsb,
    UsbAttach { busid: String, description: String, bound: bool },
    UsbDetach(String),
    Disconnect,
    PickFiles,
    AcceptOffer(u64),
    DismissOffer(u64),
    DismissTransfer(u64),
    CancelTransfer(u64),
    SetTransport(crate::net::Transport),
    OpenFolder(std::path::PathBuf),
}

/// The settings key of a policy (inverse of `parse_policy`).
pub fn policy_key(p: nya_proto::pb::BitratePolicy) -> &'static str {
    use nya_proto::pb::BitratePolicy as P;
    match p {
        P::Quality => "quality",
        P::Balanced => "balanced",
        P::Smooth => "smooth",
        P::Fixed => "fixed",
        P::Unspecified => "auto",
    }
}

pub fn parse_policy(s: &str) -> nya_proto::pb::BitratePolicy {
    use nya_proto::pb::BitratePolicy as P;
    match s {
        "quality" => P::Quality,
        "balanced" => P::Balanced,
        "smooth" => P::Smooth,
        "fixed" => P::Fixed,
        _ => P::Unspecified,
    }
}

const POLICIES: [(&str, &str, &str); 5] = [
    ("auto", "自动", "办公模式用“清晰优先”，游戏模式用“均衡”"),
    ("quality", "清晰优先", "只有持续 2 秒以上严重发送不出去才降，最低保留 60%"),
    ("balanced", "均衡", "持续积压或延迟明显上涨时降到实际能发送的速率，最低 35%"),
    ("smooth", "流畅优先", "积压、延迟上涨、丢包都会触发，最低 15%，适合很差的网络"),
    ("fixed", "固定码率", "从不自动调整"),
];

/// Controls for the host display setup; returns true if something changed.
fn display_choice_ui(ui: &mut egui::Ui, c: &mut crate::session::DisplayChoice) -> bool {
    let before = *c;
    ui.horizontal(|ui| {
        ui.label("虚拟显示器");
        for (n, label) in [(0, "不用"), (1, "1 个"), (2, "2 个"), (3, "3 个"), (4, "4 个")] {
            ui.selectable_value(&mut c.count, n, label);
        }
    })
    .response
    .on_hover_text("在被控端新建虚拟显示器（第一个设为主显示器）。多个时可在“显示器”里切换查看。需要被控端安装“虚拟显示器”组件，并以服务模式运行");
    ui.add_enabled_ui(c.count > 0, |ui| {
        ui.horizontal(|ui| {
            ui.label("被控端物理显示器");
            ui.selectable_value(&mut c.physical_off, false, "保持显示").on_hover_text("物理显示器和虚拟显示器同时存在（扩展屏）");
            ui.selectable_value(&mut c.physical_off, true, "关闭（黑屏）").on_hover_text("只保留虚拟显示器，被控端屏幕黑屏；断开后自动恢复");
        });
    });
    ui.checkbox(&mut c.block_input, "屏蔽被控端本地键盘鼠标").on_hover_text("远程操作时，被控端旁边的人无法操作（Ctrl+Alt+Del 除外）");
    if c.count == 0 {
        c.physical_off = false;
    }
    *c != before
}

fn size_text(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

const DIM: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xab);
const OK: Color32 = Color32::from_rgb(0x3c, 0xcf, 0x8e);
const WARN: Color32 = Color32::from_rgb(0xff, 0xc0, 0x5c);
const DANGER: Color32 = Color32::from_rgb(0xff, 0x8f, 0x86);
const BAR_FILL: Color32 = Color32::from_rgba_premultiplied(25, 27, 32, 235);

/// Colour of an end-to-end latency: good / fair / poor (0 = not known yet).
fn latency_color(ms: f32) -> Color32 {
    match ms {
        m if m < 60.0 => OK,
        m if m < 120.0 => WARN,
        _ => DANGER,
    }
}

fn bar_separator(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, 20.0), egui::Sense::hover());
    ui.painter().rect_filled(r, 0.0, Color32::from_white_alpha(30));
    ui.add_space(4.0);
}

/// Current display, e.g. "屏幕 2 · 虚拟".
fn display_title(s: &Session) -> String {
    let Some(info) = &s.info else { return "显示器".into() };
    let current = s.stream.as_ref().map(|x| x.display_id).unwrap_or(s.start.display_id);
    match info.displays.iter().position(|d| d.id == current) {
        Some(i) => format!("屏幕 {}{}", i + 1, if info.displays[i].is_virtual { " · 虚拟" } else { "" }),
        None => "显示器".into(),
    }
}

/// Host displays to switch to, then the display setup (virtual screens, privacy).
fn displays_menu(ui: &mut egui::Ui, s: &Session, actions: &mut Vec<Action>) {
    ui.set_min_width(330.0);
    if s.watching() {
        ui.label(RichText::new("观看时只显示被控端的主画面，切换和更改显示器由正在操作的人决定").color(DIM));
        return;
    }
    ui.label(RichText::new("被控端的显示器").small().color(DIM));
    if let Some(info) = &s.info {
        let current = s.stream.as_ref().map(|x| x.display_id).unwrap_or(s.start.display_id);
        for (i, d) in info.displays.iter().enumerate() {
            let text = format!(
                "屏幕 {} · {}{}   {}×{}{}",
                i + 1,
                if d.is_virtual { "虚拟" } else { "物理" },
                if d.primary { " · 主" } else { "" },
                d.width,
                d.height,
                if d.hdr { " · HDR→SDR" } else { "" }
            );
            ui.horizontal(|ui| {
                if ui.selectable_label(d.id == current, text).clicked() && d.id != current {
                    actions.push(Action::SelectDisplay(d.id));
                    ui.close_menu();
                }
                if s.multi_supported && d.id != current {
                    let open = s.view_of(d.id).is_some();
                    let label = if open { "窗口中" } else { "新窗口" };
                    if ui.small_button(label).on_hover_text(if open { "已在单独的窗口中显示，点击切换过去" } else { "在一个新窗口里同时显示这个屏幕" }).clicked() {
                        actions.push(Action::OpenWindow(d.id));
                        ui.close_menu();
                    }
                }
            });
        }
    }
    if s.multi_supported && s.vd_available() && s.display_choice().count < 4 {
        if ui
            .button("＋ 新建虚拟屏，在新窗口显示")
            .on_hover_text("在被控端多建一个虚拟显示器，分辨率跟随新窗口的大小。可以把几个窗口并排放在本机的同一块屏幕上")
            .clicked()
        {
            actions.push(Action::NewVirtualWindow);
            ui.close_menu();
        }
    }
    ui.separator();
    ui.label(RichText::new("显示设置").small().color(DIM));
    if s.vd_available() {
        let current = s.display_choice();
        let mut c = current;
        if display_choice_ui(ui, &mut c) {
            actions.push(Action::SetDisplayChoice(c));
        }
        ui.horizontal(|ui| {
            let private = crate::session::DisplayChoice { count: current.count.max(1), physical_off: true, block_input: true };
            if ui
                .add_enabled(current != private, egui::Button::new("一键隐私屏"))
                .on_hover_text("虚拟显示器 + 被控端物理显示器黑屏 + 屏蔽本地键鼠")
                .clicked()
            {
                actions.push(Action::SetDisplayChoice(private));
                ui.close_menu();
            }
            if ui.add_enabled(current != Default::default(), egui::Button::new("恢复被控端原样")).clicked() {
                actions.push(Action::SetDisplayChoice(Default::default()));
                ui.close_menu();
            }
        });
    } else if s.info.is_some() {
        ui.label(
            RichText::new(if s.vd_supported {
                "被控端没有安装虚拟显示器（在被控端的“本机 → 可选组件”中安装）"
            } else {
                "被控端版本太旧，不支持虚拟显示器"
            })
            .color(DIM),
        );
    }
}

/// What the small toolbar of an extra window asks for.
pub enum ExtraAction {
    None,
    ToggleFullscreen,
    Close,
}

/// Toolbar of an extra window (another host display): name, fullscreen, close.
/// Opens and hides like the main bar.
pub fn extra_overlay(ctx: &egui::Context, title: &str, status: &str, fullscreen: bool) -> ExtraAction {
    let mut action = ExtraAction::None;
    auto_hide_bar(ctx, "extra-toolbar", false, title, |ui, _| {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(title).strong().color(Color32::WHITE));
            bar_separator(ui);
            let mut fs = fullscreen;
            if ui.toggle_value(&mut fs, "全屏").changed() {
                action = ExtraAction::ToggleFullscreen;
            }
            if ui.button("关闭窗口").on_hover_text("只关闭这个窗口，不断开连接").clicked() {
                action = ExtraAction::Close;
            }
        });
    });
    if !status.is_empty() {
        egui::Area::new(egui::Id::new("extra-status"))
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE.fill(BAR_FILL).corner_radius(10).inner_margin(egui::Margin::symmetric(14, 8)).show(ui, |ui| {
                    status_label(ui, status);
                });
            });
    }
    action
}

/// Picture mode and what to do when the network gets worse.
fn mode_menu(ui: &mut egui::Ui, s: &Session, actions: &mut Vec<Action>) {
    ui.set_min_width(260.0);
    ui.label(RichText::new("画面模式（Ctrl+Alt+Shift+M）").small().color(DIM));
    let modes = [
        (false, "办公 · 清晰", "文字清晰（本机能硬件解码时用 HEVC 4:4:4），画面静止后补发清晰帧；画面不变时几乎不占带宽"),
        (true, "游戏 · 流畅", "4:2:0，高帧率（跟随本机显示器刷新率），延迟最低"),
    ];
    for (game, name, tip) in modes {
        if ui.selectable_label(s.game == game, name).on_hover_text(tip).clicked() {
            if s.game != game {
                actions.push(Action::SetGameMode(game));
            }
            ui.close_menu();
        }
    }
    ui.separator();
    ui.label(RichText::new("网络变差时").small().color(DIM));
    let current = s.bitrate_policy();
    let cur_key = POLICIES.iter().find(|p| parse_policy(p.0) as i32 == current).map(|p| p.0).unwrap_or("auto");
    for (key, name, tip) in POLICIES {
        if ui.selectable_label(key == cur_key, name).on_hover_text(tip).clicked() {
            if key != cur_key {
                actions.push(Action::SetPolicy(parse_policy(key)));
            }
            ui.close_menu();
        }
    }
    if let Some(st) = s.server_stats.as_ref().filter(|st| st.target_kbps > 0) {
        ui.label(
            RichText::new(format!("当前 {:.1} Mbps，上限 {:.1} Mbps", st.bitrate_kbps as f32 / 1000.0, st.target_kbps as f32 / 1000.0))
                .small()
                .color(DIM),
        );
    }
    ui.separator();
    let now = match s.via_tcp {
        Some(true) => "（现在：TCP）",
        Some(false) => "（现在：UDP）",
        None => "",
    };
    ui.label(RichText::new(format!("连接方式{now}")).small().color(DIM));
    use crate::net::Transport;
    let modes = [
        (Transport::Auto, "自动", "优先 UDP；UDP 连不上或丢包严重时改用 TCP，UDP 恢复后再换回来"),
        (Transport::Udp, "仅 UDP", "画面延迟最低；网络限制 UDP 时可能连不上或卡顿"),
        (Transport::Tcp, "仅 TCP", "UDP 不通或很差时用；网络差时延迟比 UDP 高"),
    ];
    for (t, name, tip) in modes {
        if ui.selectable_label(s.transport == t, name).on_hover_text(tip).clicked() {
            if s.transport != t {
                actions.push(Action::SetTransport(t));
            }
            ui.close_menu();
        }
    }
}

/// Relative mouse (locked in the window) on / off.
fn mouse_lock_toggle(ui: &mut egui::Ui, s: &Session, actions: &mut Vec<Action>) {
    let mut rel = s.relative;
    if ui.toggle_value(&mut rel, "锁定鼠标").on_hover_text("相对鼠标：鼠标锁在窗口内，适合 FPS 游戏。Ctrl+Alt+Shift+R").changed() {
        actions.push(Action::Hotkey(Hotkey::ToggleRelative));
    }
}

/// Local microphone into the host's virtual cable on / off.
fn mic_toggle(ui: &mut egui::Ui, s: &Session, actions: &mut Vec<Action>) {
    match s.host_mic_device() {
        Some(dev) => {
            let mut mic = s.mic_on();
            if ui
                .toggle_value(&mut mic, "麦克风")
                .on_hover_text(format!("本机麦克风 → 被控端“{dev}”。打开期间 CABLE Output 自动成为被控端的默认麦克风"))
                .changed()
            {
                actions.push(Action::SetMic(mic));
            }
        }
        None => {
            ui.add_enabled(false, egui::Button::new("麦克风"))
                .on_disabled_hover_text("被控端没有安装虚拟声卡（可在被控端的“本机 → 可选组件”中安装）");
        }
    }
}

/// Less frequent session controls. `compact`: the bar left out the mouse
/// lock and microphone switches (narrow window), so they are here.
fn more_menu(ui: &mut egui::Ui, s: &Session, compact: bool, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    ui.add_enabled_ui(!s.watching(), |ui| {
        if compact {
            mouse_lock_toggle(ui, s, actions);
            mic_toggle(ui, s, actions);
            ui.separator();
        }
        if ui.button("发送文件…").on_hover_text("也可以直接把文件拖进窗口").clicked() {
            actions.push(Action::PickFiles);
            ui.close_menu();
        }
        if ui.button("发送 Ctrl+Alt+Del").on_hover_text("Ctrl+Alt+Shift+D，需要被控端以服务模式运行").clicked() {
            actions.push(Action::Hotkey(Hotkey::CtrlAltDel));
            ui.close_menu();
        }
        if s.usb_available() {
            let mut open = s.usb_open;
            if ui.toggle_value(&mut open, "USB 设备透传…").changed() {
                actions.push(Action::ToggleUsb);
                ui.close_menu();
            }
        }
    });
    if let Some(g) = &s.gamepads {
        let n = g.count();
        if n > 0 {
            ui.label(RichText::new(format!("手柄 ×{n} 已映射为被控端 Xbox 手柄")).color(OK));
        }
    }
    ui.separator();
    let mut stats = s.show_stats;
    if ui.toggle_value(&mut stats, "统计信息").on_hover_text("Ctrl+Alt+Shift+S").changed() {
        actions.push(Action::Hotkey(Hotkey::ToggleStats));
    }
    ui.label(RichText::new("Ctrl+Alt+Shift+T 让工具条一直显示").small().color(DIM));
}

/// How long the pointer has to rest on the handle before the bar opens.
const HANDLE_DWELL_S: f64 = 0.25;
/// How long the bar stays after the pointer left it.
const BAR_LINGER_S: f64 = 0.4;

#[derive(Clone, Copy, Default)]
struct BarState {
    /// The bar was drawn last frame, at this rectangle.
    shown: Option<egui::Rect>,
    open_until: f64,
    menu_open: bool,
    /// The pointer has been resting on the handle since then.
    hover_since: Option<f64>,
    /// Horizontal offset from the centre (the handle can be dragged aside).
    offset_x: f32,
}

/// A bar at the top of a session window. Hidden, only a small handle with
/// `tab` remains at the top edge, so the remote picture under the bar stays
/// clickable: the bar opens when the pointer rests on the handle (or it is
/// clicked, or `pinned`), stays while the pointer is on it or one of its menus
/// is open, and hides a moment after the pointer leaves. The handle can be
/// dragged sideways out of the way; the bar opens where the handle is.
/// `contents` draws the bar and sets its `bool` when a menu of the bar is
/// open. `id` keeps separate bars apart.
pub fn auto_hide_bar(ctx: &egui::Context, id: &str, pinned: bool, tab: &str, contents: impl FnOnce(&mut egui::Ui, &mut bool)) {
    let state_id = egui::Id::new((id, "state"));
    let mut st: BarState = ctx.data(|d| d.get_temp(state_id)).unwrap_or_default();
    let now = ctx.input(|i| i.time);
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    // Only the bar as it is on screen counts; while hidden, its old place is
    // the remote picture again.
    let on_bar = matches!((pointer, st.shown), (Some(p), Some(r)) if r.expand(8.0).contains(p));
    // egui menus (display menu, "⋯") are not popups in egui's memory: the bar
    // remembers that one of its menus was open, or it would hide as soon as
    // the pointer moves down into the menu.
    let hold = pinned || on_bar || st.menu_open || ctx.memory(|m| m.any_popup_open());
    let mut show_bar = hold || now < st.open_until;
    let half = ctx.screen_rect().width() / 2.0;
    st.offset_x = st.offset_x.clamp(-(half - 60.0).max(0.0), (half - 60.0).max(0.0));

    if !show_bar {
        let r = egui::Area::new(egui::Id::new((id, "handle")))
            .anchor(Align2::CENTER_TOP, [st.offset_x, 0.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let f = egui::Frame::NONE
                    .fill(BAR_FILL)
                    .stroke(egui::Stroke::new(1.0_f32, Color32::from_white_alpha(18)))
                    .corner_radius(egui::CornerRadius { nw: 0, ne: 0, sw: 8, se: 8 })
                    .inner_margin(egui::Margin::symmetric(10, 1))
                    .show(ui, |ui| ui.add(egui::Label::new(RichText::new(format!("▾ {tab}")).small().color(DIM)).selectable(false).extend()));
                ui.interact(f.response.rect, egui::Id::new((id, "grip")), egui::Sense::click_and_drag())
            })
            .inner;
        if r.dragged() {
            st.offset_x += r.drag_delta().x;
            st.hover_since = None;
        } else if r.clicked() {
            show_bar = true;
        } else if r.hovered() && !ctx.input(|i| i.pointer.any_down()) {
            let since = *st.hover_since.get_or_insert(now);
            if now - since >= HANDLE_DWELL_S {
                show_bar = true;
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(HANDLE_DWELL_S - (now - since) + 0.01));
            }
        } else {
            st.hover_since = None;
        }
        if !show_bar {
            st.shown = None;
            st.menu_open = false;
            ctx.data_mut(|d| d.insert_temp(state_id, st));
            return;
        }
        st.hover_since = None;
        st.open_until = now + BAR_LINGER_S;
    }

    if !hold {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let mut menu_open = false;
    let bar = egui::Area::new(egui::Id::new(id))
        .anchor(Align2::CENTER_TOP, [st.offset_x, 6.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let frame = egui::Frame::NONE
                .fill(BAR_FILL)
                .stroke(egui::Stroke::new(1.0_f32, Color32::from_white_alpha(18)))
                .shadow(ui.visuals().popup_shadow);
            frame.corner_radius(12).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.spacing_mut().button_padding = egui::vec2(9.0, 5.0);
                // A horizontal row starts this tall and centres each item as it
                // is placed: as tall as the buttons, or what comes before the
                // first button (host name, latency) sits too high.
                let row = ui.text_style_height(&egui::TextStyle::Button) + 2.0 * 5.0;
                ui.spacing_mut().interact_size.y = ui.spacing().interact_size.y.max(row);
                {
                    // Flat buttons on the bar; menus keep the normal look.
                    let w = &mut ui.visuals_mut().widgets;
                    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
                    w.inactive.bg_fill = Color32::TRANSPARENT;
                    w.hovered.weak_bg_fill = Color32::from_white_alpha(22);
                    w.hovered.bg_fill = Color32::from_white_alpha(22);
                }
                contents(ui, &mut menu_open);
            });
        });
    if menu_open != st.menu_open {
        ctx.request_repaint();
    }
    if hold || menu_open {
        st.open_until = now + BAR_LINGER_S;
    }
    st.shown = Some(bar.response.rect);
    st.menu_open = menu_open;
    ctx.data_mut(|d| d.insert_temp(state_id, st));
}

/// Another client operates the host: say so, offer to take over.
fn watching_bar(ctx: &egui::Context, s: &Session, actions: &mut Vec<Action>) {
    let who = s.role.as_ref().map(|r| r.controller.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "另一个客户端".into());
    egui::Area::new(egui::Id::new("watching"))
        .anchor(Align2::CENTER_TOP, [0.0, 56.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(Color32::from_rgba_unmultiplied(28, 30, 36, 235))
                .stroke(egui::Stroke::new(1.0_f32, Color32::from_white_alpha(18)))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("正在观看 · {who} 正在操作")).color(Color32::WHITE));
                        ui.add_space(6.0);
                        if ui.button("接管操作").on_hover_text(format!("由你操作被控端，{who} 改为观看")).clicked() {
                            actions.push(Action::TakeControl(false));
                        }
                        if ui.button(RichText::new("顶掉对方").color(DANGER)).on_hover_text(format!("由你操作被控端，并断开 {who} 的连接")).clicked() {
                            actions.push(Action::TakeControl(true));
                        }
                    });
                });
        });
}

/// Toolbar and statistics shown over the remote picture.
pub fn session_overlay(
    ctx: &egui::Context,
    s: &mut Session,
    toolbar_open: bool,
    fullscreen: bool,
    hovering_file: bool,
    actions: &mut Vec<Action>,
) {
    let label = s.label.clone();
    auto_hide_bar(
        ctx,
        "toolbar",
        toolbar_open,
        &label,
        |ui, menu_open| {
            // Narrow window: the mouse lock and microphone go into "⋯".
            let compact = ui.ctx().screen_rect().width() < 760.0;
            let watching = s.watching();
            ui.horizontal(|ui| {
                // Who and how well: host name, latency (click: statistics), viewers.
                ui.add_space(8.0);
                let latency = s.summary.latency_ms;
                let color = if latency > 0.0 { latency_color(latency) } else { DIM };
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, color);
                // Name and latency as one text: one baseline.
                let mut job = egui::text::LayoutJob::default();
                let style = ui.style().clone();
                RichText::new(&s.label).strong().color(Color32::WHITE).append_to(&mut job, &style, egui::FontSelection::Default, egui::Align::BOTTOM);
                if latency > 0.0 {
                    RichText::new(format!("  {latency:.0} ms")).small().color(color).append_to(&mut job, &style, egui::FontSelection::Default, egui::Align::BOTTOM);
                }
                let r = ui.scope(|ui| {
                    ui.spacing_mut().button_padding.x = 4.0;
                    ui.add(egui::Button::new(job).frame(false))
                });
                let hint = if latency > 0.0 { "端到端延迟。点击显示 / 隐藏统计信息（Ctrl+Alt+Shift+S）" } else { "点击显示 / 隐藏统计信息（Ctrl+Alt+Shift+S）" };
                if r.inner.on_hover_text(hint).clicked() {
                    actions.push(Action::Hotkey(Hotkey::ToggleStats));
                }
                if let Some(r) = s.role.as_ref().filter(|r| r.controlling && !r.viewers.is_empty()) {
                    ui.label(RichText::new(format!("· {} 人观看", r.viewers.len())).small().color(DIM))
                        .on_hover_text(format!("正在观看：{}", r.viewers.join("、")));
                }
                bar_separator(ui);

                // The picture: which host display, and how it is streamed.
                // egui 0.31 returns a menu's contents only on the frame it closes, so
                // the menu itself reports that it is open.
                let r = ui.menu_button(format!("{} ▾", display_title(s)), |ui| {
                    *menu_open = true;
                    displays_menu(ui, s, actions)
                });
                r.response.on_hover_text("被控端的显示器、虚拟显示器和隐私屏");
                let r = ui.menu_button(format!("{} ▾", if s.game { "游戏" } else { "办公" }), |ui| {
                    *menu_open = true;
                    mode_menu(ui, s, actions)
                });
                r.response.on_hover_text("画面模式（办公 / 游戏）和网络变差时的策略");
                bar_separator(ui);

                // Input and devices sent to the host (only for the operator).
                ui.add_enabled_ui(!watching, |ui| {
                    let mut grab = input::grabbed();
                    if ui.toggle_value(&mut grab, "键盘").on_hover_text("捕获键盘：Win 键等组合键发给被控端，Ctrl+Alt+Shift+Q").changed() {
                        actions.push(Action::SetGrab(grab));
                    }
                    if !compact {
                        mouse_lock_toggle(ui, s, actions);
                        mic_toggle(ui, s, actions);
                    }
                });
                let r = ui.menu_button("⋯", |ui| {
                    *menu_open = true;
                    more_menu(ui, s, compact, actions)
                });
                r.response.on_hover_text("发送文件、Ctrl+Alt+Del、USB、统计信息");
                bar_separator(ui);

                // The window and the connection.
                let mut fs = fullscreen;
                if ui.toggle_value(&mut fs, "全屏").on_hover_text("Ctrl+Alt+Shift+F").changed() {
                    actions.push(Action::Hotkey(Hotkey::ToggleFullscreen));
                }
                if ui.button(RichText::new("断开").color(DANGER)).on_hover_text("Ctrl+Alt+Shift+X").clicked() {
                    actions.push(Action::Disconnect);
                }
                ui.add_space(4.0);
            });
        },
    );

    if s.watching() {
        watching_bar(ctx, s, actions);
    }
    if !s.status.is_empty() {
        egui::Area::new(egui::Id::new("status"))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -24.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(28, 30, 36, 235))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        status_label(ui, &s.status);
                    });
            });
    }

    transfers_panel(ctx, s, actions);
    if s.usb_open {
        usb_window(ctx, s, actions);
    }

    if hovering_file {
        egui::Area::new(egui::Id::new("drop-hint"))
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(24.0).show(ui, |ui| {
                    ui.label(RichText::new("松开鼠标，把文件发送到被控端").size(20.0).strong());
                    ui.label(RichText::new("保存在被控端的 下载\\NyaRemoteControl，并放入被控端剪贴板").weak());
                });
            });
    }

    if s.show_stats {
        let mut open = true;
        egui::Window::new("统计")
            .open(&mut open)
            .default_pos([16.0, 48.0])
            .resizable(false)
            .collapsible(true)
            .show(ctx, |ui| {
                for l in s.stats_lines() {
                    ui.label(RichText::new(l).monospace());
                }
            });
        if !open {
            actions.push(Action::Hotkey(Hotkey::ToggleStats));
        }
    }
}

/// A status message in its box: one line as wide as the text needs (an
/// area otherwise keeps the width of an earlier frame and wraps every
/// message into a narrow column), wrapped only beyond the window's width.
fn status_label(ui: &mut egui::Ui, text: &str) {
    let max = (ui.ctx().screen_rect().width() - 80.0).clamp(160.0, 900.0);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let natural = ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x);
    ui.set_min_width(natural.min(max));
    ui.set_max_width(max);
    ui.add(egui::Label::new(RichText::new(text).color(Color32::WHITE)).wrap());
}

/// Bottom-right panel: host file offers and running / finished transfers.
fn transfers_panel(ctx: &egui::Context, s: &Session, actions: &mut Vec<Action>) {
    if s.offers.is_empty() && s.transfers.is_empty() {
        return;
    }
    egui::Area::new(egui::Id::new("transfers"))
        .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            for o in &s.offers {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let total: u64 = o.files.iter().map(|f| f.size).sum();
                    let first = o.files.first().map(|f| f.name.as_str()).unwrap_or("");
                    let what = if o.files.len() == 1 { first.to_string() } else { format!("{} 等 {} 个文件", first, o.files.len()) };
                    ui.label(RichText::new("被控端复制了文件").strong());
                    ui.label(format!("{what}（{}）", size_text(total)));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("下载到本机").strong()).clicked() {
                            actions.push(Action::AcceptOffer(o.transfer_id));
                        }
                        if ui.button("忽略").clicked() {
                            actions.push(Action::DismissOffer(o.transfer_id));
                        }
                    });
                });
            }
            for t in s.transfers.iter().rev().take(4) {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(if t.upload { "↑ 发送" } else { "↓ 下载" }).strong());
                        ui.label(&t.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if matches!(t.state, TransferState::Running) {
                                if ui.small_button("取消").on_hover_text("停止这次传输，删除没传完的文件").clicked() {
                                    actions.push(Action::CancelTransfer(t.id));
                                }
                            } else if ui.small_button("✕").clicked() {
                                actions.push(Action::DismissTransfer(t.id));
                            }
                        });
                    });
                    match &t.state {
                        TransferState::Running => {
                            let frac = if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .text(format!("{} / {}", size_text(t.done), size_text(t.total)))
                                    .desired_width(340.0),
                            );
                        }
                        TransferState::Done(m) => {
                            ui.label(RichText::new(m).color(Color32::LIGHT_GREEN));
                            if let Some(f) = &t.folder {
                                if ui.button("打开文件夹").clicked() {
                                    actions.push(Action::OpenFolder(f.clone()));
                                }
                            }
                        }
                        TransferState::Failed(m) => {
                            ui.label(RichText::new(m).color(Color32::from_rgb(255, 120, 110)));
                        }
                    }
                });
            }
        });
}

fn usb_window(ctx: &egui::Context, s: &Session, actions: &mut Vec<Action>) {
    let mut open = true;
    egui::Window::new("USB 设备透传").open(&mut open).default_pos([60.0, 80.0]).default_width(460.0).show(ctx, |ui| {
        if s.usbipd_present.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在检查本机的 usbipd-win…");
            });
            return;
        }
        if s.usbipd_present == Some(false) {
            ui.label("需要在本机安装 usbipd-win（可选组件，开源免费）。");
            let running = s.usbipd_install.as_ref().is_some_and(|i| i.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("一键安装")).on_hover_text("自动下载固定版本、校验后静默安装，会弹出一次管理员权限确认").clicked() {
                    actions.push(Action::InstallUsbipd);
                }
                if ui.button("官网").clicked() {
                    let _ = std::process::Command::new("explorer").arg(crate::usb::DOWNLOAD_URL).spawn();
                }
            });
            if let Some((running, msg)) = &s.usbipd_install {
                ui.horizontal(|ui| {
                    if *running {
                        ui.spinner();
                    }
                    ui.label(msg);
                });
            }
            return;
        }
        ui.label(RichText::new("透传后设备在本机暂时不可用，断开或结束会话后自动归还。首次共享某个设备会请求一次管理员权限。").weak().small());
        if ui.button("刷新").clicked() {
            actions.push(Action::RefreshUsb);
        }
        ui.separator();
        match &s.usb_devices {
            None => {
                ui.spinner();
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(Color32::from_rgb(255, 120, 110)));
            }
            Some(Ok(list)) if list.is_empty() => {
                ui.label(RichText::new("没有检测到 USB 设备").weak());
            }
            Some(Ok(list)) => {
                egui::Grid::new("usb").num_columns(3).spacing([12.0, 8.0]).striped(true).show(ui, |ui| {
                    for d in list {
                        ui.label(format!("{}  [{}]", d.description, d.busid));
                        let (attached, msg) = s.usb_state.get(&d.busid).cloned().unwrap_or((false, String::new()));
                        if s.usb_busy.contains(&d.busid) {
                            ui.spinner();
                        } else if attached {
                            ui.label(RichText::new("已透传").color(Color32::LIGHT_GREEN));
                        } else if !msg.is_empty() {
                            ui.label(RichText::new(&msg).color(Color32::from_rgb(255, 160, 120)).small());
                        } else if d.in_use {
                            ui.label(RichText::new("正被其他 USB/IP 客户端使用").weak().small());
                        } else {
                            ui.label("");
                        }
                        ui.add_enabled_ui(!s.usb_busy.contains(&d.busid), |ui| {
                            if attached {
                                if ui.button("停止").clicked() {
                                    actions.push(Action::UsbDetach(d.busid.clone()));
                                }
                            } else if ui.button("透传").clicked() {
                                actions.push(Action::UsbAttach {
                                    busid: d.busid.clone(),
                                    description: d.description.clone(),
                                    bound: d.bound,
                                });
                            }
                        });
                        ui.end_row();
                    }
                });
            }
        }
    });
    if !open {
        actions.push(Action::ToggleUsb);
    }
}

#[cfg(test)]
mod bar_tests {
    //! The auto-hiding bar, driven headlessly with simulated pointer input.

    use super::*;
    use std::cell::Cell;

    struct Sim {
        ctx: egui::Context,
        time: f64,
        pos: egui::Pos2,
        /// What the last frame drew.
        bar: Cell<bool>,
        menu: Cell<bool>,
        button: Cell<Option<egui::Rect>>,
        menu_rect: Cell<Option<egui::Rect>>,
        /// Window width.
        width: f32,
        label: Cell<Option<egui::Rect>>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                ctx: egui::Context::default(),
                time: 0.0,
                pos: egui::pos2(500.0, 300.0),
                bar: Cell::new(false),
                menu: Cell::new(false),
                button: Cell::new(None),
                menu_rect: Cell::new(None),
                width: 1000.0,
                label: Cell::new(None),
            }
        }

        fn frame(&mut self, dt: f64, events: Vec<egui::Event>) {
            self.time += dt;
            let mut all = vec![egui::Event::PointerMoved(self.pos)];
            all.extend(events);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(self.width, 700.0))),
                time: Some(self.time),
                events: all,
                ..Default::default()
            };
            self.bar.set(false);
            self.menu.set(false);
            let _ = self.ctx.run(input, |ctx| {
                auto_hide_bar(ctx, "toolbar", false, "host", |ui, menu_open| {
                    self.bar.set(true);
                    ui.horizontal(|ui| {
                        self.label.set(Some(ui.label("host").rect));
                        let r = ui.menu_button("屏幕 1 ▾", |ui| {
                            *menu_open = true;
                            self.menu.set(true);
                            for i in 0..6 {
                                let _ = ui.button(format!("item {i}"));
                            }
                            self.menu_rect.set(Some(ui.min_rect()));
                        });
                        self.button.set(Some(r.response.rect));
                    });
                });
            });
        }

        fn move_to(&mut self, p: egui::Pos2) {
            self.pos = p;
            self.frame(0.016, vec![]);
        }

        fn click(&mut self) {
            let pos = self.pos;
            let press = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            self.frame(0.016, vec![press(true)]);
            self.frame(0.05, vec![press(false)]);
        }

        fn wait(&mut self, secs: f64) {
            for _ in 0..(secs / 0.1) as usize {
                self.frame(0.1, vec![]);
            }
        }
    }

    /// The handle at the top centre (before it is dragged).
    const HANDLE: egui::Pos2 = egui::pos2(500.0, 6.0);

    impl Sim {
        fn press(&mut self, pressed: bool) {
            let pos = self.pos;
            self.frame(0.016, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }]);
        }

        /// A window that has been showing for a moment (new egui areas
        /// spend their first frame measuring themselves).
        fn started() -> Self {
            let mut s = Self::new();
            s.frame(0.0, vec![]);
            s.wait(0.5);
            s
        }

        /// Rest on the handle until the bar opens.
        fn open_from_handle(&mut self) {
            self.move_to(HANDLE);
            self.wait(0.5);
        }
    }

    #[test]
    fn stays_while_its_menu_is_open() {
        let mut s = Sim::started();
        s.wait(1.0);
        assert!(!s.bar.get(), "hidden at first");
        s.open_from_handle();
        assert!(s.bar.get(), "opened by resting on the handle");
        let b = s.button.get().unwrap();
        s.move_to(b.center());
        s.click();
        assert!(s.menu.get(), "menu opened");
        // Down into the menu, well below the bar, and linger there.
        let m = s.menu_rect.get().unwrap();
        s.move_to(egui::pos2(m.center().x, m.bottom() - 5.0));
        s.wait(3.0);
        assert!(s.bar.get() && s.menu.get(), "bar and menu stay while the menu is open");
        // Close the menu by clicking elsewhere; the bar hides after the delay.
        s.move_to(egui::pos2(900.0, 650.0));
        s.click();
        s.wait(2.0);
        assert!(!s.menu.get() && !s.bar.get(), "hidden again after the menu closed");
    }

    #[test]
    fn hides_after_the_pointer_leaves() {
        let mut s = Sim::started();
        s.open_from_handle();
        s.move_to(egui::pos2(500.0, 20.0));
        assert!(s.bar.get());
        s.move_to(egui::pos2(500.0, 400.0));
        s.wait(1.0);
        assert!(!s.bar.get());
    }

    #[test]
    fn the_top_edge_and_a_passing_pointer_do_not_open_it() {
        let mut s = Sim::started();
        // Top edge away from the handle (e.g. a remote tab strip).
        s.move_to(egui::pos2(150.0, 1.0));
        s.wait(1.0);
        assert!(!s.bar.get());
        // Crossing the handle on the way somewhere else.
        s.move_to(HANDLE);
        s.move_to(egui::pos2(500.0, 120.0));
        s.wait(1.0);
        assert!(!s.bar.get());
    }

    #[test]
    fn where_the_bar_was_is_the_remote_picture_again() {
        let mut s = Sim::started();
        s.open_from_handle();
        let bar = s.button.get().unwrap();
        s.move_to(egui::pos2(500.0, 400.0));
        s.wait(1.0);
        assert!(!s.bar.get());
        // Below the handle, inside the area the bar covered.
        s.move_to(egui::pos2(bar.center().x, bar.bottom()));
        s.wait(1.0);
        assert!(!s.bar.get(), "the old bar area must not reopen it");
    }

    #[test]
    fn the_handle_is_one_line() {
        let mut s = Sim::started();
        let line = s.ctx.style().text_styles[&egui::TextStyle::Small].size;
        let handle = |s: &Sim| s.ctx.memory(|m| m.area_rect(egui::Id::new(("toolbar", "handle")))).expect("handle shown");
        assert!(handle(&s).height() < line * 2.0, "handle {:?} wraps (small text {line})", handle(&s));
        // The window started tiny (created hidden) and then grew.
        let mut t = Sim::new();
        t.width = 40.0;
        t.frame(0.0, vec![]);
        t.wait(0.3);
        t.width = 1000.0;
        t.wait(0.5);
        assert!(handle(&t).height() < line * 2.0, "handle {:?} wraps after the window grew", handle(&t));
        // Dragged to the right edge: little room to its right.
        s.move_to(HANDLE);
        s.press(true);
        for x in (500..1000).step_by(25) {
            s.move_to(egui::pos2(x as f32, 6.0));
        }
        s.press(false);
        s.move_to(egui::pos2(500.0, 300.0));
        s.wait(1.0);
        let r = handle(&s);
        assert!(r.right() > 900.0, "dragged: {r:?}");
        assert!(r.height() < line * 2.0, "handle {r:?} wraps at the edge (small text {line})");
    }

    /// What comes before the first button (host name) is centred with the buttons.
    #[test]
    fn the_bar_is_aligned() {
        let mut s = Sim::started();
        s.open_from_handle();
        let (label, button) = (s.label.get().unwrap(), s.button.get().unwrap());
        assert!((label.center().y - button.center().y).abs() < 1.0, "label {label:?}, button {button:?}");
    }

    #[test]
    fn a_click_on_the_handle_opens_at_once() {
        let mut s = Sim::started();
        s.move_to(HANDLE);
        s.click();
        assert!(s.bar.get());
    }

    #[test]
    fn the_handle_can_be_dragged_aside() {
        let mut s = Sim::started();
        s.move_to(HANDLE);
        s.press(true);
        for x in (200..500).rev().step_by(25) {
            s.move_to(egui::pos2(x as f32, 6.0));
            assert!(!s.bar.get(), "dragging does not open the bar");
        }
        s.press(false);
        s.move_to(egui::pos2(500.0, 300.0));
        s.wait(1.0);
        // Not at the centre any more; at the new place it opens, there.
        s.move_to(HANDLE);
        s.wait(0.5);
        assert!(!s.bar.get());
        s.move_to(egui::pos2(200.0, 6.0));
        s.wait(0.5);
        assert!(s.bar.get());
        assert!(s.button.get().unwrap().center().x < 400.0);
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    /// A status message is one line as wide as its text, frame after frame
    /// (it used to keep a narrow width and wrap into a column).
    #[test]
    fn status_is_as_wide_as_its_text() {
        let ctx = egui::Context::default();
        let text = "共享文件夹已出现在被控端的 Z 盘";
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0))),
            ..Default::default()
        };
        let mut size = egui::Vec2::ZERO;
        for _ in 0..4 {
            let _ = ctx.run(input(), |ctx| {
                let r = egui::Area::new(egui::Id::new("status"))
                    .anchor(Align2::CENTER_BOTTOM, [0.0, -24.0])
                    .show(ctx, |ui| status_label(ui, text));
                size = r.response.rect.size();
            });
        }
        // (Not ctx.style() inside ctx.fonts(): nested context locks deadlock.)
        let font = egui::TextStyle::Body.resolve(&ctx.style());
        let line = ctx.fonts(|f| f.row_height(&font));
        assert!(size.y < line * 1.5, "one line, got {size:?}");
        let natural = ctx.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x);
        assert!(size.x >= natural - 1.0, "as wide as the text ({natural}), got {size:?}");
    }
}
