use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

use gdk;
use glib;
use gtk;
use neovim_lib::neovim::Neovim;
use neovim_lib::neovim_api::NeovimApi;
use pango;

use gtk::prelude::*;

use nvim_bridge::{GnvimEvent, ModeInfo, Notify, OptionSet, RedrawEvent};
use thread_guard::ThreadGuard;
use ui::cmdline::Cmdline;
use ui::color::{Color, Highlight};
use ui::font::Font;
use ui::grid::Grid;
use ui::popupmenu::Popupmenu;
use ui::tabline::Tabline;

type Grids = HashMap<u64, Grid>;

#[derive(Default)]
pub struct HlDefs {
    hl_defs: HashMap<u64, Highlight>,

    pub default_fg: Color,
    pub default_bg: Color,
    pub default_sp: Color,
}

impl HlDefs {
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut Highlight> {
        self.hl_defs.get_mut(id)
    }

    pub fn get(&self, id: &u64) -> Option<&Highlight> {
        self.hl_defs.get(id)
    }

    pub fn insert(&mut self, id: u64, hl: Highlight) -> Option<Highlight> {
        self.hl_defs.insert(id, hl)
    }
}

/// Internal structure for `UI` to work on.
struct UIState {
    /// All grids currently in the UI.
    grids: Grids,
    /// Highlight definitions. This is shared across all grids.
    hl_defs: Arc<Mutex<HlDefs>>,
    /// Mode infos. When a mode is activated, the activated mode is passed
    /// to the gird(s).
    mode_infos: Vec<ModeInfo>,
    /// Id of the current active grid.
    current_grid: u64,

    popupmenu: Popupmenu,
    cmdline: Cmdline,
    tabline: Tabline,

    /// Overlay contains our grid(s) and popupmenu.
    #[allow(unused)]
    overlay: gtk::Overlay,

    /// Source id for delayed call to ui_try_resize.
    resize_source_id: Arc<Mutex<Option<glib::SourceId>>>,
}

/// Main UI structure.
pub struct UI {
    /// Main window.
    win: Arc<ThreadGuard<gtk::ApplicationWindow>>,
    /// Neovim instance.
    nvim: Arc<Mutex<Neovim>>,
    /// Channel to receive event from nvim.
    rx: Receiver<Notify>,
    /// Our internal state, containing basically everything we manipulate
    /// when we receive an event from nvim.
    state: Arc<ThreadGuard<UIState>>,
}

impl UI {
    /// Creates new UI.
    ///
    /// * `app` - GTK application for the UI.
    /// * `rx` - Channel to receive nvim UI events.
    /// * `nvim` - Neovim instance to use. Should be the same that is the source
    ///            of `rx` events.
    pub fn init(
        app: &gtk::Application,
        rx: Receiver<Notify>,
        nvim: Arc<Mutex<Neovim>>,
    ) -> Self {
        // Create the main window.
        let window = gtk::ApplicationWindow::new(app);
        window.set_title("Neovim");
        window.set_default_size(1280, 720);

        // Top level widget.
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&b);

        let tabline = Tabline::new(nvim.clone());
        b.pack_start(&tabline.get_widget(), false, false, 0);

        // Our root widget.
        let overlay = gtk::Overlay::new();
        b.pack_start(&overlay, true, true, 0);

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        overlay.add(&box_);

        // Create hl defs and initialize 0th element because we'll need to have
        // something that is accessible for the default grid that we're gonna
        // make next.
        let mut hl_defs = HlDefs::default();
        hl_defs.insert(0, Highlight::default());
        let hl_defs = Arc::new(Mutex::new(hl_defs));

        // Create default grid.
        let mut grid = Grid::new(1, hl_defs.clone());
        box_.pack_start(&grid.widget(), true, true, 0);

        // When resizing our window (main grid), we'll have to tell neovim to
        // resize it self also. The notify to nvim is send with a small delay,
        // so we don't spam it multiple times a second. source_id is used to
        // track the function timeout.
        let source_id = Arc::new(Mutex::new(None));
        let source_id_ref = source_id.clone();
        let nvim_ref = nvim.clone();
        grid.connect_da_resize(move |rows, cols| {
            let nvim_ref = nvim_ref.clone();

            let source_id_moved = source_id_ref.clone();
            // Set timeout to notify nvim about the new size.
            let new = glib::timeout_add(30, move || {
                let mut nvim = nvim_ref.lock().unwrap();
                nvim.ui_try_resize(cols as i64, rows as i64).unwrap();

                // Set the source_id to none, so we don't accidentally remove
                // it since it used at this point.
                let source_id = source_id_moved.clone();
                let mut source_id = source_id.lock().unwrap();
                *source_id = None;

                Continue(false)
            });

            let source_id = source_id_ref.clone();
            let mut source_id = source_id.lock().unwrap();
            {
                // If we have earlier timeout, remove it.
                let old = source_id.take();
                if let Some(old) = old {
                    glib::source::source_remove(old);
                }
            }

            *source_id = Some(new);

            false
        });

        // Mouse button press event.
        let nvim_ref = nvim.clone();
        grid.connect_mouse_button_press_events(move |button, row, col| {
            let mut nvim = nvim_ref.lock().unwrap();
            let input = format!("<{}Mouse><{},{}>", button, col, row);
            nvim.input(&input).expect("Couldn't send mouse input");

            Inhibit(false)
        });

        // Mouse button release events.
        let nvim_ref = nvim.clone();
        grid.connect_mouse_button_release_events(move |button, row, col| {
            let mut nvim = nvim_ref.lock().unwrap();
            let input = format!("<{}Release><{},{}>", button, col, row);
            nvim.input(&input).expect("Couldn't send mouse input");

            Inhibit(false)
        });

        // Mouse drag events.
        let nvim_ref = nvim.clone();
        grid.connect_motion_events_for_drag(move |button, row, col| {
            let mut nvim = nvim_ref.lock().unwrap();
            let input = format!("<{}Drag><{},{}>", button, col, row);
            nvim.input(&input).expect("Couldn't send mouse input");

            Inhibit(false)
        });

        // Scrolling events.
        let nvim_ref = nvim.clone();
        grid.connect_scroll_events(move |dir, row, col| {
            let mut nvim = nvim_ref.lock().unwrap();
            let input = format!("<{}><{},{}>", dir, col, row);
            nvim.input(&input).expect("Couldn't send mouse input");

            Inhibit(false)
        });

        // IMMulticontext is used to handle most of the inputs.
        let im_context = gtk::IMMulticontext::new();
        let nvim_ref = nvim.clone();
        im_context.set_use_preedit(false);
        im_context.connect_commit(move |_, input| {
            // "<" needs to be escaped for nvim.input()
            let nvim_input = input.replace("<", "<lt>");

            let mut nvim = nvim_ref.lock().unwrap();
            nvim.input(&nvim_input).expect("Couldn't send input");
        });

        let im_ref = im_context.clone();
        let nvim_ref = nvim.clone();
        window.connect_key_press_event(move |_, e| {
            if im_ref.filter_keypress(e) {
                Inhibit(true)
            } else {
                if let Some(input) = event_to_nvim_input(e) {
                    let mut nvim = nvim_ref.lock().unwrap();
                    nvim.input(input.as_str()).expect("Couldn't send input");
                    return Inhibit(true);
                } else {
                    println!(
                        "Failed to turn input event into nvim key (keyval: {})",
                        e.get_keyval()
                    )
                }

                Inhibit(false)
            }
        });

        let im_ref = im_context.clone();
        window.connect_key_release_event(move |_, e| {
            im_ref.filter_keypress(e);
            Inhibit(false)
        });

        let im_ref = im_context.clone();
        window.connect_focus_in_event(move |_, _| {
            im_ref.focus_in();
            Inhibit(false)
        });

        let im_ref = im_context.clone();
        window.connect_focus_out_event(move |_, _| {
            im_ref.focus_out();
            Inhibit(false)
        });

        let cmdline = Cmdline::new(&overlay, hl_defs.clone(), nvim.clone());

        window.show_all();

        grid.set_im_context(&im_context);

        cmdline.hide();

        let mut grids = HashMap::new();
        grids.insert(1, grid);

        UI {
            win: Arc::new(ThreadGuard::new(window)),
            rx,
            state: Arc::new(ThreadGuard::new(UIState {
                grids: grids,
                mode_infos: vec![],
                current_grid: 1,
                popupmenu: Popupmenu::new(&overlay, nvim.clone()),
                cmdline,
                overlay,
                tabline,
                resize_source_id: source_id,
                hl_defs,
            })),
            nvim,
        }
    }

    /// Starts to listen events from `rx` (e.g. from nvim) and processing those.
    /// Think this as the "main" function of the UI.
    pub fn start(self) {
        let rx = self.rx;
        let state = self.state.clone();
        let win = self.win.clone();
        let nvim = self.nvim.clone();

        thread::spawn(move || {
            let timeout = time::Duration::from_millis(33);

            loop {
                // Use timeout, so we can use this loop to "tick" the current
                // grid (mainly to just blink the cursor).
                let notify = rx.recv_timeout(timeout);

                if let Err(RecvTimeoutError::Disconnected) = notify {
                    // Neovim closed and the sender is disconnected
                    // so we need to exit too.
                    break;
                }

                let state = state.clone();
                let nvim = nvim.clone();
                glib::idle_add(move || {
                    let mut state = state.borrow_mut();

                    // Handle any events that we might have.
                    if let Ok(ref notify) = notify {
                        handle_notify(notify, &mut state, nvim.clone());
                    }

                    // Tick the current active grid.
                    let grid = state.grids.get(&state.current_grid).unwrap();
                    grid.tick();

                    glib::Continue(false)
                });
            }

            // Close the window once the recv loop exits.
            glib::idle_add(move || {
                win.borrow().close();
                glib::Continue(false)
            });
        });
    }
}

fn handle_notify(
    notify: &Notify,
    state: &mut UIState,
    nvim: Arc<Mutex<Neovim>>,
) {
    match notify {
        Notify::RedrawEvent(events) => {
            handle_redraw_event(events, state, nvim);
        }
        Notify::GnvimEvent(event) => {
            handle_gnvim_event(event, state);
        }
    }
}

fn handle_gnvim_event(event: &GnvimEvent, state: &mut UIState) {
    match event {
        GnvimEvent::SetGuiColors(colors) => {
            state.popupmenu.set_colors(colors.pmenu);
            state.tabline.set_colors(colors.tabline);
            state.cmdline.set_colors(colors.cmdline);
            state.cmdline.wildmenu_set_colors(&colors.wildmenu);
        }
        GnvimEvent::CompletionMenuToggleInfo => {
            state.popupmenu.toggle_show_info()
        }
        GnvimEvent::Unknown(msg) => {
            println!("Received unknown GnvimEvent: {}", msg);
        }
    }
}

fn handle_redraw_event(
    events: &Vec<RedrawEvent>,
    state: &mut UIState,
    nvim: Arc<Mutex<Neovim>>,
) {
    for event in events {
        match event {
            RedrawEvent::GridLine(lines) => {
                for line in lines {
                    let grid = state.grids.get(&line.grid).unwrap();
                    grid.put_line(line);
                }
            }
            RedrawEvent::GridCursorGoto(grid_id, row, col) => {
                // Gird cursor goto sets the current cursor to grid_id,
                // so we'll need to handle that here...
                let grid = if *grid_id != state.current_grid {
                    // ...so if the grid_id is not same as the state tells us,
                    // set the previous current grid to inactive state.
                    state
                        .grids
                        .get(&state.current_grid)
                        .unwrap()
                        .set_active(false);
                    state.current_grid = *grid_id;

                    // And set the new current grid to active.
                    let grid = state.grids.get(grid_id).unwrap();
                    grid.set_active(true);
                    grid
                } else {
                    state.grids.get(grid_id).unwrap()
                };

                // And after all that, set the current grid's cursor position.
                grid.cursor_goto(*row, *col);
            }
            RedrawEvent::GridResize(grid, width, height) => {
                let grid = state.grids.get(grid).unwrap();
                grid.resize(*width, *height);
            }
            RedrawEvent::GridClear(grid) => {
                let grid = state.grids.get(grid).unwrap();
                grid.clear();
            }
            RedrawEvent::GridScroll(grid, reg, rows, cols) => {
                let grid = state.grids.get(grid).unwrap();
                grid.scroll(*reg, *rows, *cols);
            }
            RedrawEvent::DefaultColorsSet(fg, bg, sp) => {
                let mut hl_defs = state.hl_defs.lock().unwrap();
                hl_defs.default_fg = *fg;
                hl_defs.default_bg = *bg;
                hl_defs.default_sp = *sp;

                // NOTE(ville): Not sure if these are actually needed.
                let hl = hl_defs.get_mut(&0).unwrap();
                hl.foreground = Some(*fg);
                hl.background = Some(*bg);
                hl.special = Some(*sp);
            }
            RedrawEvent::HlAttrDefine(defs) => {
                let mut hl_defs = state.hl_defs.lock().unwrap();

                for (id, hl) in defs {
                    hl_defs.insert(*id, *hl);
                }
            }
            RedrawEvent::OptionSet(opts) => {
                for opt in opts {
                    match opt {
                        OptionSet::GuiFont(font) => {
                            let font = Font::from_guifont(font)
                                .unwrap_or(Font::default());
                            let pango_font = font.as_pango_font();

                            for grid in (state.grids).values() {
                                grid.set_font(pango_font.clone());
                            }

                            // Cancel any possible delayed call for ui_try_resize.
                            let mut id = state.resize_source_id.lock().unwrap();
                            if let Some(id) = id.take() {
                                glib::source::source_remove(id);
                            }

                            // Channing the font affects the grid size, so we'll
                            // need to tell nvim our new size.
                            let grid = state.grids.get(&1).unwrap();
                            let (rows, cols) = grid.calc_size();
                            let mut nvim = nvim.lock().unwrap();
                            nvim.ui_try_resize(cols as i64, rows as i64)
                                .unwrap();

                            state.popupmenu.set_font(font.clone());
                            state.cmdline.set_font(font.clone());
                            state.tabline.set_font(font.clone());
                        }
                        OptionSet::NotSupported(name) => {
                            println!("Not supported option set: {}", name);
                        }
                    }
                }
            }
            RedrawEvent::ModeInfoSet(_cursor_shape_enabled, infos) => {
                state.mode_infos = infos.clone();
            }
            RedrawEvent::ModeChange(_name, idx) => {
                let mode = state.mode_infos.get(*idx as usize).unwrap();
                // Broadcast the mode change to all grids.
                // TODO(ville): It might be enough to just set the mode to the
                //              current active grid.
                for grid in state.grids.values() {
                    grid.set_mode(mode);
                }
            }
            RedrawEvent::SetBusy(busy) => {
                for grid in state.grids.values() {
                    grid.set_busy(*busy);
                }
            }
            RedrawEvent::Flush() => {
                for grid in state.grids.values() {
                    grid.flush();
                }
            }
            RedrawEvent::PopupmenuShow(popupmenu) => {
                state.popupmenu.set_items(popupmenu.items.clone());

                let grid = state.grids.get(&state.current_grid).unwrap();
                let mut rect =
                    grid.get_rect_for_cell(popupmenu.row, popupmenu.col);

                let extra_h = state.tabline.get_height();
                println!("H: {}", extra_h);
                rect.y -= extra_h;

                state.popupmenu.set_anchor(rect);
                state.popupmenu.show();
            }
            RedrawEvent::PopupmenuHide() => {
                state.popupmenu.hide();
            }
            RedrawEvent::PopupmenuSelect(selected) => {
                state.popupmenu.select(*selected as i32);
            }
            RedrawEvent::TablineUpdate(cur, tabs) => {
                state.tabline.update(cur.clone(), tabs.clone());
            }
            RedrawEvent::CmdlineShow(cmdline_show) => {
                state.cmdline.show(cmdline_show);
            }
            RedrawEvent::CmdlineHide() => {
                state.cmdline.hide();
            }
            RedrawEvent::CmdlinePos(pos, level) => {
                state.cmdline.set_pos(*pos, *level);
            }
            RedrawEvent::CmdlineSpecialChar(ch, shift, level) => {
                state.cmdline.show_special_char(ch.clone(), *shift, *level);
            }
            RedrawEvent::CmdlineBlockShow(lines) => {
                state.cmdline.show_block(lines);
            }
            RedrawEvent::CmdlineBlockAppend(line) => {
                state.cmdline.block_append(line);
            }
            RedrawEvent::CmdlineBlockHide() => {
                state.cmdline.hide_block();
            }
            RedrawEvent::WildmenuShow(items) => {
                state.cmdline.wildmenu_show(items);
            }
            RedrawEvent::WildmenuHide() => {
                state.cmdline.wildmenu_hide();
            }
            RedrawEvent::WildmenuSelect(item) => {
                state.cmdline.wildmenu_select(*item);
            }
            RedrawEvent::Unknown(e) => {
                println!("Received unknown redraw event: {}", e);
            }
        }
    }
}

fn keyname_to_nvim_key(s: &str) -> Option<&str> {
    // Sourced from python-gui.
    match s {
        "slash" => Some("/"),
        "backslash" => Some("\\"),
        "dead_circumflex" => Some("^"),
        "at" => Some("@"),
        "numbersign" => Some("#"),
        "dollar" => Some("$"),
        "percent" => Some("%"),
        "ampersand" => Some("&"),
        "asterisk" => Some("*"),
        "parenleft" => Some("("),
        "parenright" => Some(")"),
        "underscore" => Some("_"),
        "plus" => Some("+"),
        "minus" => Some("-"),
        "bracketleft" => Some("["),
        "bracketright" => Some("]"),
        "braceleft" => Some("{"),
        "braceright" => Some("}"),
        "dead_diaeresis" => Some("\""),
        "dead_acute" => Some("\'"),
        "less" => Some("<"),
        "greater" => Some(">"),
        "comma" => Some(","),
        "period" => Some("."),
        "BackSpace" => Some("BS"),
        "Return" => Some("CR"),
        "Escape" => Some("Esc"),
        "Delete" => Some("Del"),
        "Page_Up" => Some("PageUp"),
        "Page_Down" => Some("PageDown"),
        "Enter" => Some("CR"),
        "ISO_Left_Tab" => Some("Tab"),
        "Tab" => Some("Tab"),
        "Up" => Some("Up"),
        "Down" => Some("Down"),
        "Left" => Some("Left"),
        "Right" => Some("Right"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        _ => None,
    }
}

fn event_to_nvim_input(e: &gdk::EventKey) -> Option<String> {
    let mut input = String::from("");

    let keyval = e.get_keyval();
    let keyname = gdk::keyval_name(keyval)?;

    let state = e.get_state();

    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        input.push_str("S-");
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        input.push_str("C-");
    }
    if state.contains(gdk::ModifierType::MOD1_MASK) {
        input.push_str("A-");
    }

    if keyname.chars().count() > 1 {
        let n = keyname_to_nvim_key(keyname.as_str())?;
        input.push_str(n);
    } else {
        input.push(gdk::keyval_to_unicode(keyval)?);
    }

    Some(format!("<{}>", input))
}
