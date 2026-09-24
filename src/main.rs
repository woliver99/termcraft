mod block;
mod entity;
mod game;
mod game3;
mod input;
mod net;
mod render;
mod render3;
mod world;
mod world3;

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use game::Game;
use game3::{Game3, InputMode};
use net::{Net, NetOpts};
use world3::{D3, H3, W3};

const TICK: Duration = Duration::from_millis(50); // 20 TPS

const HELP: &str = "termcraft - a Minecraft-inspired sandbox for your terminal

USAGE:
  termcraft [OPTIONS]

OPTIONS:
  --2d           Play the classic 2D side-view mode
  --creative     3D creative mode: always fly, no gravity or fall damage
  --new          Start a fresh world (ignores the saved one)
  --seed <N>     Shared world seed - see MULTIPLAYER below
  --name <NAME>  Name other players see (default: $USER)
  --solo         Stay single player even with --seed
  --open         Let other machines on your LAN join this world
  --join <ADDR>  Join a world on another machine (host[:port])
  --diag, -d     Run input & keyboard protocol diagnostics (troubleshoot Hold Mode)
  --help         Show this help
  --version      Show version

The default mode is first-person 3D. Worlds autosave to
~/.termcraft/save3d.json (3D), ~/.termcraft/world-<seed>.json (shared
seeds) and ~/.termcraft/save.json (2D) on quit.

MULTIPLAYER:
  Run `termcraft --seed 42` in two terminals and you're in the same world:
  the first one hosts it, the second joins and gets the host's copy. You
  see each other's avatars, share every block that gets mined or placed,
  chat with `t`, list players with Tab, and can punch each other with `x`.
  Only the host writes the save file. `--open` plus `--join <ip>` does the
  same across a LAN.

3D CONTROLS:
  w / a / s / d  move (relative to where you're looking)
  arrow keys     look around (or drag the mouse)
  space          jump (swim up in water); rise in --creative
  f              descend (creative mode only)
  x / Enter      mine the block under the crosshair (or left-click),
                 or punch the player you're aiming at
  z              place selected block against the targeted face
  1-9            select hotbar slot
  c              crafting menu
  t              chat (multiplayer)
  Tab            player list
  F5 / Ctrl+S    save
  q / Esc        quit

CREATIVE (--creative, 3D only):
  Always flying: no gravity, no fall damage, still collides with blocks.
  w/a/s/d fly relative to look (including pitch). space rises, f descends.

2D CONTROLS (--2d):
  a / d          move left / right
  w / space      jump (swim up in water)
  arrow keys     aim the target cursor
  x / Enter      mine block / attack zombie (or left-click)
  z              place selected block (or right-click)";

/// Reads the value that follows a flag, or exits with a usage error.
fn need_value(args: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    match args.get(*i) {
        Some(v) => v.clone(),
        None => {
            eprintln!("error: {flag} requires a value");
            std::process::exit(2);
        }
    }
}

fn restore_terminal() {
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    {
        use std::io::Write;
        let _ = io::stdout().write_all(b"\x1b[?9001l");
        if std::env::var_os("TMUX").is_some() {
            let _ = io::stdout().write_all(b"\x1bPtmux;\x1b\x1b[<u\x1b\\");
            let _ = io::stdout().write_all(b"\x1bPtmux;\x1b\x1b[?9001l\x1b\\");
        }
        let _ = io::stdout().flush();
    }
    let _ = disable_raw_mode();
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut force_new = false;
    let mut mode_2d = false;
    let mut creative = false;
    let mut seed: Option<u64> = None;
    let mut solo = false;
    let mut net_opts = NetOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("termcraft {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--diag" | "-d" => {
                run_diagnostics()?;
                return Ok(());
            }
            "--2d" => mode_2d = true,
            "--creative" => creative = true,
            "--new" => force_new = true,
            "--solo" => solo = true,
            "--open" => net_opts.open = true,
            "--name" => net_opts.name = need_value(&args, &mut i, "--name"),
            "--join" => net_opts.join = Some(need_value(&args, &mut i, "--join")),
            "--seed" => {
                let raw = need_value(&args, &mut i, "--seed");
                match raw.parse::<u64>() {
                    Ok(v) => seed = Some(v),
                    Err(_) => {
                        eprintln!("error: --seed requires a number");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("error: unknown option '{other}' (try --help)");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // Always restore the terminal, even if we panic mid-frame.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    if mode_2d {
        let mut game = if force_new {
            Game::new(seed.unwrap_or_else(default_seed))
        } else {
            Game::load().unwrap_or_else(|| Game::new(default_seed()))
        };
        let (mut terminal, hold_keys) = setup_terminal()?;
        game.set_hold_mode(hold_keys);
        let result = run(&mut terminal, &mut game);
        restore_terminal();
        match game.save() {
            Ok(()) => println!("World saved to {}", game::save_path().display()),
            Err(e) => eprintln!("Failed to save world: {e}"),
        }
        println!("Thanks for playing TermCraft!");
        result
    } else {
        if net_opts.join.is_some() && seed.is_none() {
            eprintln!("error: --join also needs --seed <N> (the world you're joining)");
            std::process::exit(2);
        }
        // An explicit seed means "share this world"; --solo opts out.
        let shared = if solo { None } else { seed };
        let mut game = match shared {
            Some(seed) => match start_multiplayer(seed, &net_opts, force_new) {
                Ok(game) => game,
                Err(e) => {
                    eprintln!("Multiplayer unavailable ({e}); starting single player.");
                    Game3::new(seed)
                }
            },
            None => match seed {
                Some(seed) => Game3::new(seed),
                None if force_new => Game3::new(default_seed()),
                None => Game3::load().unwrap_or_else(|| Game3::new(default_seed())),
            },
        };
        if creative {
            game.set_creative(true);
        }
        let (mut terminal, hold_keys) = setup_terminal()?;
        game.set_hold_mode(hold_keys);
        let result = run3(&mut terminal, &mut game);
        restore_terminal();
        let _ = game.save_player();
        if game.owns_save() {
            match game.save() {
                Ok(()) => println!("World saved to {}", game.save_file().display()),
                Err(e) => eprintln!("Failed to save world: {e}"),
            }
        } else {
            println!("Player progress saved! (The host keeps the world terrain).");
        }
        println!("Thanks for playing TermCraft!");
        result
    }
}

/// Hosts or joins the session for `seed` and returns a game bound to it.
///
/// Whoever gets there first hosts (and owns the save file); everyone else
/// downloads that host's world so all edits line up.
fn start_multiplayer(seed: u64, opts: &NetOpts, force_new: bool) -> Result<Game3, String> {
    let mut net = Net::start(seed, opts).map_err(|e| e.to_string())?;
    let path = game3::seed_save_path(seed);
    let mut game = if net.is_authority() {
        // Shared seeds are sticky: pick up where the world left off.
        let saved = if force_new {
            None
        } else {
            Game3::load_from(&path)
        };
        saved.unwrap_or_else(|| Game3::new(seed))
    } else {
        let (bytes, spawn, time) = net.await_world((W3 * H3 * D3) as usize)?;
        let world = world3::World3::from_bytes(&bytes, spawn)
            .ok_or_else(|| "the host sent a world we can't read".to_string())?;
        let mut game = Game3::from_world(seed, world);
        game.time = time;
        game
    };
    let owns = net.is_authority();
    game.set_save(path, owns);
    game.attach_net(net);
    if !force_new && game.load_player() {
        game.say("Player progress loaded. Welcome back!");
    }
    Ok(game)
}

/// Returns the terminal plus whether key release events are available
/// (kitty keyboard protocol), enabling true hold-to-move.
fn setup_terminal() -> io::Result<(Terminal<CrosstermBackend<io::Stdout>>, bool)> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    if std::env::var_os("TMUX").is_some() {
        use std::io::Write;
        let _ = io::stdout().write_all(b"\x1bPtmux;\x1b\x1b[>1u\x1b\\");
        let _ = io::stdout().write_all(b"\x1bPtmux;\x1b\x1b[?9001h\x1b\\");
        let _ = io::stdout().flush();
    }
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
    );
    {
        use std::io::Write;
        let _ = io::stdout().write_all(b"\x1b[?9001h");
        let _ = io::stdout().flush();
    }
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);

    // Drain any leftover terminal query responses (such as Primary Device Attributes \x1b[?61;...c
    // sent by Windows Terminal) so they aren't parsed as keystrokes when the game loop starts.
    let mut drain_buf = [0u8; 1024];
    while input::poll_stdin(Duration::from_millis(30)).unwrap_or(false) {
        use std::io::Read;
        let _ = io::stdin().read(&mut drain_buf);
    }

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok((terminal, enhanced))
}

fn default_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(42)
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, game: &mut Game) -> io::Result<()> {
    let mut reader = input::InputReader::new();
    let mut auto_promoted = false;
    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|f| render::draw(f, game))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if input::poll_stdin(timeout)? {
            reader.read_available()?;
            while let Some(ev) = reader.next_event() {
                if reader.saw_kitty && !game.saw_kitty {
                    game.saw_kitty = true;
                }
                if reader.saw_win32 && !game.saw_win32 {
                    game.saw_win32 = true;
                }
                if !auto_promoted && (reader.saw_kitty || reader.saw_win32) {
                    auto_promoted = true;
                    if game.input_mode == InputMode::Auto {
                        let label = if reader.saw_win32 { "Win32" } else { "Kitty" };
                        game.say(&format!("{label} input detected: Auto Hold Mode active!"));
                    }
                }
                match ev {
                    Event::Key(k) => game.on_key(k),
                    Event::Mouse(m) => game.on_mouse(m),
                    _ => {}
                }
            }
        }

        if last_tick.elapsed() >= TICK {
            game.tick();
            last_tick = Instant::now();
        }

        if game.should_quit {
            return Ok(());
        }
    }
}

fn run3(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, game: &mut Game3) -> io::Result<()> {
    let mut reader = input::InputReader::new();
    let mut auto_promoted = false;
    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|f| render3::draw(f, game))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if input::poll_stdin(timeout)? {
            reader.read_available()?;
            while let Some(ev) = reader.next_event() {
                if reader.saw_kitty && !game.saw_kitty {
                    game.saw_kitty = true;
                }
                if reader.saw_win32 && !game.saw_win32 {
                    game.saw_win32 = true;
                }
                if !auto_promoted && (reader.saw_kitty || reader.saw_win32) {
                    auto_promoted = true;
                    if game.input_mode == InputMode::Auto {
                        let label = if reader.saw_win32 { "Win32" } else { "Kitty" };
                        game.say(&format!("{label} input detected: Auto Hold Mode active!"));
                    }
                }
                match ev {
                    Event::Key(k) => game.on_key(k),
                    Event::Mouse(m) => game.on_mouse(m),
                    _ => {}
                }
            }
        }

        if last_tick.elapsed() >= TICK {
            game.tick();
            last_tick = Instant::now();
        }

        if game.should_quit {
            return Ok(());
        }
    }
}

fn run_diagnostics() -> io::Result<()> {
    use std::io::{Read, Write};

    println!("\x1b[1;36m========================================================================\x1b[0m");
    println!("\x1b[1;36m Terminal Party - Input & Keyboard Protocol Diagnostics 🔍\x1b[0m");
    println!("\x1b[1;36m========================================================================\x1b[0m");

    // 1. Environment Dump
    println!("\x1b[1;33m[1/3] Environment Information:\x1b[0m");
    println!("  • Platform:          {}", std::env::consts::OS);
    println!("  • TERM:              {}", std::env::var("TERM").unwrap_or_else(|_| "<unset>".into()));
    println!("  • COLORTERM:         {}", std::env::var("COLORTERM").unwrap_or_else(|_| "<unset>".into()));
    let wt = std::env::var("WT_SESSION").is_ok();
    println!("  • Windows Terminal:  {}", if wt { "YES (WT_SESSION detected)" } else { "NO (or over SSH without WT_SESSION passed)" });
    if let Ok(ssh) = std::env::var("SSH_CONNECTION") {
        println!("  • SSH Connection:    {}", ssh);
    }
    if let Ok(client) = std::env::var("SSH_CLIENT") {
        println!("  • SSH Client:        {}", client);
    }
    if let Ok(tmux) = std::env::var("TMUX") {
        println!("  • Inside TMUX:       YES ({})", tmux);
    } else {
        println!("  • Inside TMUX:       NO");
    }

    // 2. Protocol Support Check
    println!("\n\x1b[1;33m[2/3] Protocol Support Check:\x1b[0m");
    let supports = supports_keyboard_enhancement().unwrap_or(false);
    if supports {
        println!("  • Kitty Keyboard Protocol: \x1b[1;32mDetected & Supported!\x1b[0m");
    } else {
        println!("  • Kitty Keyboard Protocol: \x1b[1;31mNot detected (no response to CSI ? u)\x1b[0m");
    }

    // 3. Interactive Raw Input Inspector
    println!("\n\x1b[1;33m[3/3] Live Input Stream Monitor:\x1b[0m");
    println!("  Activating Kitty enhancement (\\x1b[>1u)...");
    println!("  Activating Win32 input mode (\\x1b[?9001h)...");
    println!("\n  \x1b[1;37mInstructions:\x1b[0m");
    println!("    1. Press and \x1b[1;32mHOLD 'W'\x1b[0m for 1-2 seconds, then \x1b[1;31mRELEASE\x1b[0m it.");
    println!("    2. Press and \x1b[1;32mHOLD an Arrow Key\x1b[0m (Left/Right/Up/Down), then \x1b[1;31mRELEASE\x1b[0m it.");
    println!("    3. Press '\x1b[1;33mq\x1b[0m' or '\x1b[1;33mEsc\x1b[0m' when finished to exit.\n");

    enable_raw_mode()?;
    {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[>1u\x1b[?9001h");
        let _ = out.flush();
    }

    print!("\r  {:<12} | {:<28} | {}\r\n", "ELAPSED", "PROTOCOL / EVENT", "RAW BYTES");
    print!("\r  {:-<12}-+-{:-<28}-+-{:-<30}\r\n", "", "", "");
    let _ = io::stdout().flush();

    let start_time = Instant::now();
    let mut saw_kitty_release = false;
    let mut saw_win32_release = false;
    let mut _saw_win32_press = false;
    let mut saw_plain_press = false;
    let mut repeat_count = 0;
    let mut last_key_byte = 0u8;

    let mut buf = [0u8; 256];
    loop {
        let n = match io::stdin().read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let slice = &buf[..n];
        let elapsed = format!("+{}ms", start_time.elapsed().as_millis());

        // Check if exit (q, Q, Esc, Ctrl+C in raw, ANSI, or Win32 format)
        let is_exit = slice == b"q"
            || slice == b"Q"
            || slice == b"\x1b"
            || slice.contains(&0x03)
            || slice.starts_with(b"\x1b[81;") // Win32 Vk=81 ('Q')
            || slice.starts_with(b"\x1b[27;"); // Win32 Vk=27 (Esc)
        if is_exit {
            break;
        }

        // Check for Win32 input mode sequence: \x1b[Vk;Sc;Uc;Kd;Cs;Rc_
        if slice.starts_with(b"\x1b[") && slice.ends_with(b"_") {
            if let Ok(s) = std::str::from_utf8(&slice[2..slice.len() - 1]) {
                let parts: Vec<&str> = s.split(';').collect();
                if parts.len() >= 4 {
                    let vk = parts[0];
                    let kd = parts[3];
                    let is_press = kd == "1";
                    if is_press {
                        _saw_win32_press = true;
                        print!("\r  {:<12} | \x1b[1;32mWIN32 KEYDOWN (Vk={})\x1b[0m   | {:?}\r\n", elapsed, vk, slice);
                    } else {
                        saw_win32_release = true;
                        print!("\r  {:<12} | \x1b[1;31mWIN32 KEYUP (Vk={})\x1b[0m     | {:?}\r\n", elapsed, vk, slice);
                    }
                    let _ = io::stdout().flush();
                    continue;
                }
            }
        }

        // Check for Kitty keyboard protocol: \x1b[...u or \x1b[...~ with event type
        if slice.starts_with(b"\x1b[") && (slice.ends_with(b"u") || slice.ends_with(b"~")) {
            let is_release = slice.windows(2).any(|w| w == b":3" || w == b";3");
            let is_repeat = slice.windows(2).any(|w| w == b":2" || w == b";2");
            if is_release {
                saw_kitty_release = true;
                print!("\r  {:<12} | \x1b[1;31mKITTY RELEASE\x1b[0m           | {:?}\r\n", elapsed, slice);
            } else if is_repeat {
                print!("\r  {:<12} | \x1b[33mKITTY REPEAT\x1b[0m            | {:?}\r\n", elapsed, slice);
            } else {
                print!("\r  {:<12} | \x1b[1;32mKITTY PRESS\x1b[0m             | {:?}\r\n", elapsed, slice);
            }
            let _ = io::stdout().flush();
            continue;
        }

        // ANSI escape sequence (e.g. arrow keys \x1b[A, \x1b[B, \x1b[C, \x1b[D)
        if slice.starts_with(b"\x1b[") {
            let name = match slice {
                b"\x1b[A" => "Arrow Up",
                b"\x1b[B" => "Arrow Down",
                b"\x1b[C" => "Arrow Right",
                b"\x1b[D" => "Arrow Left",
                _ => "ANSI Escape Sequence",
            };
            print!("\r  {:<12} | \x1b[36mANSI: {:<18}\x1b[0m | {:?}\r\n", elapsed, name, slice);
            let _ = io::stdout().flush();
            continue;
        }

        // Plain byte (e.g. 'w', 'a', 's', 'd', Space)
        if slice.len() == 1 {
            let b = slice[0];
            saw_plain_press = true;
            if b == last_key_byte {
                repeat_count += 1;
                print!("\r  {:<12} | \x1b[33mTYPEMATIC REPEAT '{}'\x1b[0m    | [0x{:02x}] (repeat #{})\r\n", elapsed, b as char, b, repeat_count);
            } else {
                last_key_byte = b;
                repeat_count = 0;
                let ch = if b.is_ascii_graphic() { b as char } else { ' ' };
                print!("\r  {:<12} | \x1b[1;32mPLAIN BYTE '{}'\x1b[0m         | [0x{:02x}]\r\n", elapsed, ch, b);
            }
            let _ = io::stdout().flush();
            continue;
        }

        // Any other bytes
        print!("\r  {:<12} | RAW BYTES                    | {:?}\r\n", elapsed, slice);
        let _ = io::stdout().flush();
    }

    // Teardown raw mode & protocols
    {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[<u\x1b[?9001l");
        let _ = out.flush();
    }
    let _ = disable_raw_mode();

    println!("\r\n\x1b[1;36m========================================================================\x1b[0m");
    println!("\x1b[1;36m Diagnostics Summary & Findings 📋\x1b[0m");
    println!("\x1b[1;36m========================================================================\x1b[0m");

    if saw_kitty_release {
        println!("\x1b[1;32m✅ Kitty Keyboard Protocol is fully functional!\x1b[0m");
        println!("   Key release events are delivered. Hold Mode works natively.");
    } else if saw_win32_release {
        println!("\x1b[1;32m✅ Win32 Input Mode is supported by your terminal!\x1b[0m");
        println!("   Windows Terminal emitted native KeyDown/KeyUp release records (CSI ... _).");
    } else if saw_plain_press {
        println!("\x1b[1;31m❌ NO KEY RELEASE EVENTS RECEIVED!\x1b[0m");
        println!("   Your terminal emulator (or SSH path) only sends raw keypresses & typematic repeats.");
        println!("   When you hold a key, Windows emits repeated characters ('w', 'w', 'w'...) with NO release on key-up.");
        println!("   \x1b[1;33mResult:\x1b[0m In this environment, \x1b[1;36mToggle Mode (Compatibility Mode)\x1b[0m is the intended");
        println!("   and recommended way to play (tap W to walk, S to stop, Arrow keys to turn).");
        println!("   Alternatively, using an emulator like Alacritty, WezTerm, or Windows Terminal Preview 1.25+");
        println!("   will provide native Kitty protocol release events.");
    }

    println!("\x1b[1;36m========================================================================\x1b[0m\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn opts(name: &str) -> NetOpts {
        NetOpts {
            name: name.to_string(),
            join: None,
            open: false,
        }
    }

    /// Full stack: two games start on the same seed, one hosts, the other
    /// downloads that world, and edits and positions flow between them.
    #[test]
    fn two_terminals_on_one_seed_share_a_world() {
        let seed = 0xD00D_1234_5678_9ABC;
        let mut host = start_multiplayer(seed, &opts("host"), true).expect("hosts the seed");
        assert!(host.owns_save(), "the host owns the world file");

        // The guest blocks on the world snapshot, which only arrives while the
        // host is ticking, so join from another thread.
        let joining = std::thread::spawn(move || start_multiplayer(seed, &opts("guest"), true));
        let mut guest = loop {
            host.tick();
            if joining.is_finished() {
                break joining.join().unwrap().expect("guest joins");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(!guest.owns_save(), "guests must not own the save file");

        // Same world: spot-check a column of blocks.
        for y in 0..H3 {
            assert_eq!(
                host.world.get(W3 / 2, y, D3 / 2),
                guest.world.get(W3 / 2, y, D3 / 2),
                "worlds diverge at y={y}"
            );
        }

        // Let both settle and exchange positions.
        for _ in 0..40 {
            host.tick();
            guest.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(host.peers.len(), 1, "host should see the guest");
        assert_eq!(guest.peers.len(), 1, "guest should see the host");

        // Step the guest a few blocks clear of the host, otherwise looking
        // down aims at the host's avatar (which is a punch, not a dig).
        let (gx, gz) = (guest.px + 4.0, guest.pz);
        let ground = guest.world.height_at(gx as i32, gz as i32);
        guest.px = gx;
        guest.pz = gz;
        guest.py = ground as f32 + 1.0;
        for _ in 0..20 {
            host.tick();
            guest.tick();
        }

        // The guest mines a block; the host's world must follow.
        guest.pitch = -1.4;
        assert!(
            guest.peer_in_crosshair().is_none(),
            "expected clear line of sight to the ground"
        );
        let dug = guest.target().expect("ground under the guest");
        guest.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(guest.world.get(dug.x, dug.y, dug.z), block::Block::Air);
        let mut synced = false;
        for _ in 0..60 {
            host.tick();
            guest.tick();
            if host.world.get(dug.x, dug.y, dug.z) == block::Block::Air {
                synced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(synced, "the guest's dig never reached the host");

        // And chat travels the other way.
        host.chat_open = true;
        for c in "hello".chars() {
            host.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        host.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let mut heard = false;
        for _ in 0..60 {
            host.tick();
            guest.tick();
            if guest
                .recent_chat()
                .iter()
                .any(|(from, text, _)| from == "host" && text == "hello")
            {
                heard = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(heard, "the guest never received the host's chat line");
    }

    #[test]
    fn test_host_failover_and_reconnection() {
        let seed = 0xD00D_1234_5678_4321;
        let mut host = start_multiplayer(seed, &opts("host1"), true).expect("hosts the seed");
        assert!(host.owns_save(), "original host should own save");

        let joining = std::thread::spawn(move || start_multiplayer(seed, &opts("guest1"), true));
        let mut guest1 = loop {
            host.tick();
            if joining.is_finished() {
                break joining.join().unwrap().expect("guest1 joins");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(!guest1.owns_save(), "guest1 initially should not own save");

        // Let them sync initial states
        for _ in 0..20 {
            host.tick();
            guest1.tick();
            std::thread::sleep(Duration::from_millis(2));
        }

        // Drop the original host
        drop(host);

        // Guest1 ticks, detects disconnect, and automatically migrates to become Host
        for _ in 0..20 {
            guest1.tick();
            if guest1.owns_save() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(guest1.owns_save(), "guest1 should have promoted to Host");
        assert!(guest1.is_multiplayer(), "guest1 should remain in multiplayer");

        // Now a new guest2 joins the promoted host (guest1)
        let joining2 = std::thread::spawn(move || start_multiplayer(seed, &opts("guest2"), true));
        let mut guest2 = loop {
            guest1.tick();
            if joining2.is_finished() {
                break joining2.join().unwrap().expect("guest2 joins new host");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(!guest2.owns_save());
        assert!(guest2.is_multiplayer());

        // Verify communication between promoted host (guest1) and new guest (guest2)
        guest1.chat_open = true;
        for c in "failover_success".chars() {
            guest1.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        guest1.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let mut heard = false;
        for _ in 0..60 {
            guest1.tick();
            guest2.tick();
            if guest2
                .recent_chat()
                .iter()
                .any(|(from, text, _)| from == "guest1" && text == "failover_success")
            {
                heard = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(heard, "guest2 never received the promoted host's chat");
    }
}
