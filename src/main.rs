//       _____                    _____                    _____                _____          
//      /\    \                  /\    \                  /\    \              |\    \         
//     /::\    \                /::\    \                /::\    \             |:\____\        
//     \:::\    \              /::::\    \              /::::\    \            |::|   |        
//      \:::\    \            /::::::\    \            /::::::\    \           |::|   |        
//       \:::\    \          /:::/\:::\    \          /:::/\:::\    \          |::|   |        
//        \:::\    \        /:::/__\:::\    \        /:::/__\:::\    \         |::|   |        
//        /::::\    \      /::::\   \:::\    \       \:::\   \:::\    \        |::|   |        
//       /::::::\    \    /::::::\   \:::\    \    ___\:::\   \:::\    \       |::|___|______  
//      /:::/\:::\    \  /:::/\:::\   \:::\    \  /\   \:::\   \:::\    \      /::::::::\    \ 
//     /:::/  \:::\____\/:::/  \:::\   \:::\____\/::\   \:::\   \:::\____\    /::::::::::\____\
//    /:::/    \::/    /\::/    \:::\  /:::/    /\:::\   \:::\   \::/    /   /:::/~~~~/~~      
//   /:::/    / \/____/  \/____/ \:::\/:::/    /  \:::\   \:::\   \/____/   /:::/    /         
//  /:::/    /                    \::::::/    /    \:::\   \:::\    \      /:::/    /          
// /:::/    /                      \::::/    /      \:::\   \:::\____\    /:::/    /           
// \::/    /                       /:::/    /        \:::\  /:::/    /    \::/    /            
//  \/____/                       /:::/    /          \:::\/:::/    /      \/____/             
//                               /:::/    /            \::::::/    /                           
//                              /:::/    /              \::::/    /                            
//                              \::/    /                \::/    /                             
//                               \/____/                  \/____/                                                                                                                                        
                                                                                            

//! TASY v2 — Tiny Autonomous Sentient Yield
//!
//! Эволюционное расширение v1 (Embassy / ESP32-C3):
//!   + Эмоциональная система (stress, loneliness, curiosity, stability, Mood)
//!   + Memory с типом, важностью, timestamp (заменяет простой Message log)
//!   + Event bus (lock-free ring buffer, no_alloc)
//!   + Personality (сохраняется, влияет на поведение)
//!   + Cognition task (автономный когнитивный цикл)
//!   + Sleep / wake (автономный и ручной)
//!   + Primitive reinforcement (attachment растёт при пробуждении из одиночества)
//!   + SAVE_VERSION = 2 (миграция сбрасывает состояние при несовместимой схеме)

#![no_std]
#![no_main]

use core::mem::MaybeUninit;

use embassy_executor::Spawner;
use embassy_time::{Duration, Ticker, Timer};
use embedded_io_async::{Read, Write};
use embedded_storage::{ReadStorage, Storage};
use esp_backtrace as _;
use esp_hal::{
    gpio::Io,
    ledc::{
        channel::{self, Channel},
        timer::{self, LSClockSource},
        Ledc, LowSpeed,
    },
    prelude::*,
    usb_serial_jtag::UsbSerialJtag,
};
use esp_storage::FlashStorage;
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const NAME_CAP:        usize = 32;
const MEMORY_CAP:      usize = 32;
const MEMORY_TEXT_CAP: usize = 64;

/// Увеличиваем версию — несовместимая схема, сброс при upgrade
const SAVE_MAGIC:   u32 = 0x54535959;
const SAVE_VERSION: u32 = 2;

/// Сколько секунд без взаимодействия до начала роста одиночества
const LONELINESS_ONSET_SECS: u32 = 60;
/// Сколько секунд без взаимодействия до автономного перехода в сон
const SLEEP_ONSET_SECS: u32 = 300;
/// Максимум для эмоциональных параметров
const STAT_MAX: u32 = 100;

// ─────────────────────────────────────────────────────────────────────────────
// Global Atomics — состояние, видимое всем задачам
// ─────────────────────────────────────────────────────────────────────────────

// Физиология
static SYNC_RATE:  AtomicU32 = AtomicU32::new(100);
static ENERGY:     AtomicU32 = AtomicU32::new(100);
static INTEGRITY:  AtomicU32 = AtomicU32::new(100);
static BPM:        AtomicU32 = AtomicU32::new(60);

// Эмоциональное состояние
static STRESS:        AtomicU32 = AtomicU32::new(0);
static LONELINESS:    AtomicU32 = AtomicU32::new(0);
static CURIOSITY_ST:  AtomicU32 = AtomicU32::new(50);
static STABILITY:     AtomicU32 = AtomicU32::new(100);
static MOOD:          AtomicU32 = AtomicU32::new(0); // кодируется как Mood as u32

// Время
static SESSION_SECONDS:  AtomicU32 = AtomicU32::new(0);
static BOOT_COUNT:       AtomicU32 = AtomicU32::new(0);
static LAST_INTERACTION: AtomicU32 = AtomicU32::new(0);

// Режимы
static IS_SLEEPING:     AtomicBool = AtomicBool::new(false);
static SHOULD_EXIT:     AtomicBool = AtomicBool::new(false);
/// Флаг: следующий render_ui должен показать ВСЕ memories
static SHOW_ALL_MEM:    AtomicBool = AtomicBool::new(false);

// ─────────────────────────────────────────────────────────────────────────────
// Mood
// ─────────────────────────────────────────────────────────────────────────────

#[repr(u32)]
#[derive(Copy, Clone, PartialEq)]
enum Mood {
    Calm       = 0,
    Curious    = 1,
    Lonely     = 2,
    Distressed = 3,
    Exhausted  = 4,
}

impl Mood {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Mood::Curious,
            2 => Mood::Lonely,
            3 => Mood::Distressed,
            4 => Mood::Exhausted,
            _ => Mood::Calm,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Mood::Calm       => "Calm",
            Mood::Curious    => "Curious",
            Mood::Lonely     => "Lonely",
            Mood::Distressed => "Distressed",
            Mood::Exhausted  => "Exhausted",
        }
    }
}

/// Приоритетная логика: Exhausted > Distressed > Lonely > Curious > Calm
fn compute_mood() -> Mood {
    let energy     = ENERGY.load(Ordering::Relaxed);
    let stress     = STRESS.load(Ordering::Relaxed);
    let loneliness = LONELINESS.load(Ordering::Relaxed);
    let curiosity  = CURIOSITY_ST.load(Ordering::Relaxed);

    if energy < 20                       { return Mood::Exhausted;  }
    if stress > 60                       { return Mood::Distressed; }
    if loneliness > 60                   { return Mood::Lonely;     }
    if curiosity > 70 && stress < 30     { return Mood::Curious;    }
    Mood::Calm
}

// ─────────────────────────────────────────────────────────────────────────────
// Memory System
// ─────────────────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Copy, Clone)]
enum MemoryKind {
    UserInteraction = 0,
    InternalEvent   = 1,
    StressEvent     = 2,
    LearnedPattern  = 3,
    StateTransition = 4,
}

impl MemoryKind {
    fn label(self) -> &'static str {
        match self {
            MemoryKind::UserInteraction => "USER",
            MemoryKind::InternalEvent   => "INTL",
            MemoryKind::StressEvent     => "STRSS",
            MemoryKind::LearnedPattern  => "LEARN",
            MemoryKind::StateTransition => "TRANS",
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => MemoryKind::InternalEvent,
            2 => MemoryKind::StressEvent,
            3 => MemoryKind::LearnedPattern,
            4 => MemoryKind::StateTransition,
            _ => MemoryKind::UserInteraction,
        }
    }
}

/// 4 + 1 + 1 + 1 + 1 + 64 = 72 bytes;  32 memories = 2304 bytes
#[repr(C)]
#[derive(Copy, Clone)]
struct Memory {
    timestamp:  u32,
    importance: u8,
    kind:       u8,
    len:        u8,
    _pad:       u8,
    text:       [u8; MEMORY_TEXT_CAP],
}

impl Memory {
    const fn empty() -> Self {
        Self { timestamp: 0, importance: 0, kind: 0, len: 0, _pad: 0,
               text: [0; MEMORY_TEXT_CAP] }
    }
}

static mut MEMORIES:     [Memory; MEMORY_CAP] = [Memory::empty(); MEMORY_CAP];
static mut MEMORY_COUNT: usize = 0;

/// Вставить запись памяти. При переполнении вытесняет oldest.
/// Вызывается из cooperative-async контекста — race-free.
fn push_memory(text: &str, kind: MemoryKind, importance: u8) {
    let bytes = text.as_bytes();
    let len   = bytes.len().min(MEMORY_TEXT_CAP);
    let ts    = SESSION_SECONDS.load(Ordering::Relaxed);

    unsafe {
        if MEMORY_COUNT >= MEMORY_CAP {
            // сдвигаем влево — O(N) но N=32, приемлемо
            for i in 1..MEMORY_CAP {
                MEMORIES[i - 1] = MEMORIES[i];
            }
            MEMORY_COUNT = MEMORY_CAP - 1;
        }
        let idx = MEMORY_COUNT;
        MEMORIES[idx] = Memory::empty();
        MEMORIES[idx].timestamp  = ts;
        MEMORIES[idx].importance = importance;
        MEMORIES[idx].kind       = kind as u8;
        MEMORIES[idx].len        = len as u8;
        MEMORIES[idx].text[..len].copy_from_slice(&bytes[..len]);
        MEMORY_COUNT += 1;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Event Bus  —  lock-free ring buffer, no allocation
// ─────────────────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Copy, Clone, PartialEq)]
enum Event {
    TickMinute    = 0,
    UserMessage   = 1,
    UserRename    = 2,
    IntegrityLost = 3,
    SyncLost      = 4,
    MemoryCreated = 5,
    SleepEntered  = 6,
    WakeUp        = 7,
}

impl Event {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Event::TickMinute),
            1 => Some(Event::UserMessage),
            2 => Some(Event::UserRename),
            3 => Some(Event::IntegrityLost),
            4 => Some(Event::SyncLost),
            5 => Some(Event::MemoryCreated),
            6 => Some(Event::SleepEntered),
            7 => Some(Event::WakeUp),
            _ => None,
        }
    }
}

const EVT_CAP: usize = 16;

static mut EVENT_BUF: [u8; EVT_CAP] = [0u8; EVT_CAP];
static EVENT_HEAD:    AtomicU32 = AtomicU32::new(0);
static EVENT_TAIL:    AtomicU32 = AtomicU32::new(0);

/// Положить событие в очередь. Если очередь полна — drop.
fn event_push(e: Event) {
    let tail = EVENT_TAIL.load(Ordering::Relaxed);
    let next = (tail + 1) % EVT_CAP as u32;
    if next == EVENT_HEAD.load(Ordering::Acquire) { return; } // full
    unsafe { EVENT_BUF[tail as usize] = e as u8; }
    EVENT_TAIL.store(next, Ordering::Release);
}

/// Извлечь событие из очереди.
fn event_pop() -> Option<Event> {
    let head = EVENT_HEAD.load(Ordering::Acquire);
    let tail = EVENT_TAIL.load(Ordering::Relaxed);
    if head == tail { return None; }
    let raw = unsafe { EVENT_BUF[head as usize] };
    EVENT_HEAD.store((head + 1) % EVT_CAP as u32, Ordering::Release);
    Event::from_u8(raw)
}

// ─────────────────────────────────────────────────────────────────────────────
// Personality — сохраняется в flash, влияет на поведение
// ─────────────────────────────────────────────────────────────────────────────

/// curiosity  → baseline для CURIOSITY_ST
/// attachment → скорость роста loneliness
/// aggression → усилитель stress
/// optimism   → скорость пассивного восстановления energy
#[repr(C)]
#[derive(Copy, Clone)]
struct Personality {
    curiosity:  u8,
    attachment: u8,
    aggression: u8,
    optimism:   u8,
}

static mut PERSONALITY: Personality = Personality {
    curiosity:  60,
    attachment: 70,
    aggression: 20,
    optimism:   65,
};

// ─────────────────────────────────────────────────────────────────────────────
// Persistent Storage Schema  (v2)
// Размер: ~2404 байт < 4096 байт (один сектор flash)
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone)]
struct SaveData {
    // Header
    magic:    u32,   // 0
    version:  u32,   // 4
    checksum: u32,   // 8
    // Physiology
    sync_rate:  u32, // 12
    energy:     u32, // 16
    integrity:  u32, // 20
    bpm:        u32, // 24
    // Emotional
    stress:        u32, // 28
    loneliness:    u32, // 32
    curiosity_st:  u32, // 36
    stability:     u32, // 40
    mood:          u32, // 44
    // Time
    session_seconds:  u32, // 48
    boot_count:       u32, // 52
    last_interaction: u32, // 56
    // Counts (packed)
    name_len:     u8,    // 60
    memory_count: u8,    // 61
    _pad:         [u8; 2], // 62
    // Bulk data
    entity_name: [u8; NAME_CAP], // 64  (+32 = 96)
    personality: Personality,    // 96  (+4  = 100)
    memories:    [Memory; MEMORY_CAP], // 100 (+2304 = 2404)
}

impl SaveData {
    const fn default_state() -> Self {
        Self {
            magic: SAVE_MAGIC, version: SAVE_VERSION, checksum: 0,
            sync_rate: 100, energy: 100, integrity: 100, bpm: 60,
            stress: 0, loneliness: 0, curiosity_st: 50, stability: 100, mood: 0,
            session_seconds: 0, boot_count: 0, last_interaction: 0,
            name_len: 0, memory_count: 0, _pad: [0; 2],
            entity_name: [0; NAME_CAP],
            personality: Personality { curiosity: 60, attachment: 70,
                                       aggression: 20, optimism: 65 },
            memories: [Memory::empty(); MEMORY_CAP],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entity Name + LED timer (static storage обязателен для 'static ссылок)
// ─────────────────────────────────────────────────────────────────────────────

static mut ENTITY_NAME:     [u8; NAME_CAP] = [0; NAME_CAP];
static mut ENTITY_NAME_LEN: usize = 0;

static mut TIMER_STORAGE: MaybeUninit<timer::Timer<'static, LowSpeed>> =
    MaybeUninit::uninit();

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

#[main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    let timer_group =
        esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timer_group.timer0);

    let _io = Io::new(peripherals.IO_MUX);

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(esp_hal::ledc::LSGlobalClkSource::APBClk);

    let timer_raw = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    let timer = unsafe {
        let ptr = core::ptr::addr_of_mut!(TIMER_STORAGE);
        (*ptr).as_mut_ptr().write(timer_raw);
        &mut *(*ptr).as_mut_ptr()
    };

    timer.configure(timer::config::Config {
        duty:         timer::config::Duty::Duty8Bit,
        clock_source: LSClockSource::APBClk,
        frequency:    5.kHz(),
    }).unwrap();

    let mut led_channel =
        ledc.channel(channel::Number::Channel0, peripherals.GPIO8);
    led_channel.configure(channel::config::Config {
        timer,
        duty_pct:   0,
        pin_config: channel::config::PinConfig::PushPull,
    }).unwrap();

    let usb_serial =
        UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();
    let (mut rx, mut tx) = usb_serial.split();

    print_logo(&mut tx).await;

    // Just for vibe man.
    Timer::after_millis(500).await;

    let mut flash = FlashStorage::new();
    load_state(&mut flash);

    unsafe {
        if ENTITY_NAME_LEN == 0 { set_entity_name("TASY"); }
    }

    BOOT_COUNT.fetch_add(1, Ordering::Relaxed);
    // Считаем boot как свежее взаимодействие
    LAST_INTERACTION.store(SESSION_SECONDS.load(Ordering::Relaxed), Ordering::Relaxed);

    push_memory("System boot", MemoryKind::StateTransition, 80);
    event_push(Event::WakeUp);
    save_state(&mut flash);

    spawner.spawn(session_task()).unwrap();
    spawner.spawn(metabolism_task()).unwrap();
    spawner.spawn(heartbeat_task(led_channel)).unwrap();
    spawner.spawn(cognition_task()).unwrap();

    render_ui(&mut tx).await;
    let _ = tx.write_all(b"\r\nCommand > ").await;

    let mut input = [0u8; 128];
    let mut pos   = 0usize;

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            let _ = tx.write_all(b"\r\n[TASY] shutting down...\r\n").await;
            save_state(&mut flash);
            loop { Timer::after_secs(1).await; }
        }

        let mut byte = [0u8; 1];
        let got = embassy_time::with_timeout(
            Duration::from_millis(100),
            rx.read_exact(&mut byte),
        ).await.is_ok();

        if got {
            match byte[0] {
                b'\r' | b'\n' => {
                    let _ = tx.write_all(b"\r\n").await;
                    if pos > 0 {
                        let cmd = core::str::from_utf8(&input[..pos]).unwrap_or("");
                        let now = SESSION_SECONDS.load(Ordering::Relaxed);

                        // Любой ввод — взаимодействие
                        LAST_INTERACTION.store(now, Ordering::Relaxed);

                        // Пробуждение + reinforcement
                        if IS_SLEEPING.load(Ordering::Relaxed) {
                            IS_SLEEPING.store(false, Ordering::Relaxed);
                            event_push(Event::WakeUp);

                            // Primitive reinforcement: если сущность была одинока,
                            // привязанность слегка растёт
                            let loneliness = LONELINESS.load(Ordering::Relaxed);
                            if loneliness > 40 {
                                unsafe {
                                    PERSONALITY.attachment =
                                        PERSONALITY.attachment.saturating_add(1).min(100);
                                }
                                push_memory(
                                    "Learned: user returns",
                                    MemoryKind::LearnedPattern,
                                    90,
                                );
                            }
                        }

                        handle_command(cmd);
                        save_state(&mut flash);
                        pos = 0;
                        render_ui(&mut tx).await;
                        let _ = tx.write_all(b"\r\nCommand > ").await;
                    }
                }

                8 | 127 => {
                    if pos > 0 {
                        pos -= 1;
                        let _ = tx.write_all(b"\x08 \x08").await;
                    }
                }

                b => {
                    if pos < input.len() - 1 {
                        input[pos] = b;
                        pos += 1;
                        let _ = tx.write_all(&[b]).await;
                    }
                }
            }
        }

        Timer::after_millis(10).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Render UI
// ─────────────────────────────────────────────────────────────────────────────

async fn print_logo<W>(tx: &mut W)
where
    W: embedded_io_async::Write,
{
    let logo = br#"
      _____                    _____                    _____                _____          
     /\    \                  /\    \                  /\    \              |\    \         
    /::\    \                /::\    \                /::\    \             |:\____\        
    \:::\    \              /::::\    \              /::::\    \            |::|   |        
     \:::\    \            /::::::\    \            /::::::\    \           |::|   |        
      \:::\    \          /:::/\:::\    \          /:::/\:::\    \          |::|   |        
       \:::\    \        /:::/__\:::\    \        /:::/__\:::\    \         |::|   |        
       /::::\    \      /::::\   \:::\    \       \:::\   \:::\    \        |::|   |        
      /::::::\    \    /::::::\   \:::\    \    ___\:::\   \:::\    \       |::|___|______  
     /:::/\:::\    \  /:::/\:::\   \:::\    \  /\   \:::\   \:::\    \      /::::::::\    \ 
    /:::/  \:::\____\/:::/  \:::\   \:::\____\/::\   \:::\   \:::\____\    /::::::::::\____\
   /:::/    \::/    /\::/    \:::\  /:::/    /\:::\   \:::\   \::/    /   /:::/~~~~/~~      
  /:::/    / \/____/  \/____/ \:::\/:::/    /  \:::\   \:::\   \/____/   /:::/    /         
 /:::/    /                    \::::::/    /    \:::\   \:::\    \      /:::/    /          
/:::/    /                      \::::/    /      \:::\   \:::\____\    /:::/    /           
\::/    /                       /:::/    /        \:::\  /:::/    /    \::/    /            
 \/____/                       /:::/    /          \:::\/:::/    /      \/____/             
                              /:::/    /            \::::::/    /                           
                             /:::/    /              \::::/    /                            
                             \::/    /                \::/    /                             
                              \/____/                  \/____/                              
                                                                                                                         
"#;

    let _ = tx.write_all(b"\x1B[2J\x1B[H").await;
    let _ = tx.write_all(logo).await;
    let _ = tx.write_all(b"\r\n").await;
}


// ─────────────────────────────────────────────────────────────────────────────
// Render UI
// ─────────────────────────────────────────────────────────────────────────────

async fn render_ui<W: Write>(tx: &mut W) {
    let sync       = SYNC_RATE.load(Ordering::Relaxed);
    let energy     = ENERGY.load(Ordering::Relaxed);
    let integrity  = INTEGRITY.load(Ordering::Relaxed);
    let bpm        = BPM.load(Ordering::Relaxed);
    let stress     = STRESS.load(Ordering::Relaxed);
    let loneliness = LONELINESS.load(Ordering::Relaxed);
    let curiosity  = CURIOSITY_ST.load(Ordering::Relaxed);
    let stability  = STABILITY.load(Ordering::Relaxed);
    let mood       = Mood::from_u32(MOOD.load(Ordering::Relaxed));
    let session    = SESSION_SECONDS.load(Ordering::Relaxed);
    let boot       = BOOT_COUNT.load(Ordering::Relaxed);
    let sleeping   = IS_SLEEPING.load(Ordering::Relaxed);

    // Захватываем флаг — сбрасываем атомарно
    let show_all = SHOW_ALL_MEM.swap(false, Ordering::Relaxed);

    let (name_bytes, name_len) = unsafe {
        (ENTITY_NAME, ENTITY_NAME_LEN.min(NAME_CAP))
    };
    let name = core::str::from_utf8(&name_bytes[..name_len]).unwrap_or("TASY");

    let _ = tx.write_all(b"\x1B[2J\x1B[H").await;

    // ── Header ────────────────────────────────────────────────────────────
    let _ = tx.write_all(b"==================================================\r\n").await;
    let header = if sleeping {
        b"           [ TASY v2 - DORMANT              ]    \r\n" as &[u8]
    } else {
        b"           [ TASY v2 - AUTONOMOUS CORE      ]    \r\n"
    };
    let _ = tx.write_all(header).await;
    let _ = tx.write_all(b"==================================================\r\n").await;

    // ── Identity ──────────────────────────────────────────────────────────
    let _ = tx.write_all(b"  Entity   : ").await;
    let _ = tx.write_all(name.as_bytes()).await;
    let _ = tx.write_all(b"\r\n").await;

    let _ = tx.write_all(b"  Mood     : ").await;
    let _ = tx.write_all(mood.label().as_bytes()).await;
    let _ = tx.write_all(b"\r\n").await;

    let _ = tx.write_all(b"--------------------------------------------------\r\n").await;

    // ── Physiology ────────────────────────────────────────────────────────
    macro_rules! stat_line {
        ($label:expr, $val:expr, $suffix:expr) => {
            let _ = tx.write_all($label).await;
            write_u32(tx, $val).await;
            let _ = tx.write_all($suffix).await;
        };
    }

    stat_line!(b"  Sync     : ", sync,      b" %\r\n");
    stat_line!(b"  Energy   : ", energy,    b" %\r\n");
    stat_line!(b"  Integrity: ", integrity, b" %\r\n");

    let _ = tx.write_all(b"--------------------------------------------------\r\n").await;

    // ── Emotional ─────────────────────────────────────────────────────────
    stat_line!(b"  Stress   : ", stress,    b"\r\n");
    stat_line!(b"  Loneliness: ",loneliness,b"\r\n");
    stat_line!(b"  Curiosity : ",curiosity, b"\r\n");
    stat_line!(b"  Stability : ",stability, b"\r\n");

    let _ = tx.write_all(b"--------------------------------------------------\r\n").await;

    // ── System ────────────────────────────────────────────────────────────
    stat_line!(b"  BPM      : ", bpm,     b"\r\n");
    stat_line!(b"  Boot #   : ", boot,    b"\r\n");
    stat_line!(b"  Session  : ", session, b" s\r\n");

    let _ = tx.write_all(b"==================================================\r\n").await;

    // ── Memory Log ────────────────────────────────────────────────────────
    let mem_count = unsafe { MEMORY_COUNT.min(MEMORY_CAP) };

    if show_all {
        let _ = tx.write_all(b"  [ All Memories ]\r\n").await;
    } else {
        let _ = tx.write_all(b"  [ Recent Memory ]\r\n").await;
    }

    let start = if show_all {
        0
    } else if mem_count > 8 { mem_count - 8 } else { 0 };

    for i in start..mem_count {
        let m = unsafe { MEMORIES[i] };
        if m.len == 0 { continue; }

        let kind = MemoryKind::from_u8(m.kind);
        let text = core::str::from_utf8(&m.text[..m.len as usize]).unwrap_or("?");

        let _ = tx.write_all(b"  [").await;
        write_u32(tx, m.timestamp).await;
        let _ = tx.write_all(b"s|").await;
        let _ = tx.write_all(kind.label().as_bytes()).await;
        let _ = tx.write_all(b"|").await;
        write_u32(tx, m.importance as u32).await;
        let _ = tx.write_all(b"] ").await;
        let _ = tx.write_all(text.as_bytes()).await;
        let _ = tx.write_all(b"\r\n").await;
    }

    let _ = tx.write_all(b"==================================================\r\n").await;
    let _ = tx.write_all(b"  name  msg  status  memories  heal  boost\r\n").await;
    let _ = tx.write_all(b"  sleep  wake  exit\r\n").await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn write_u32<W: Write>(tx: &mut W, v: u32) {
    let mut buf = [0u8; 10];
    let s = u32_to_ascii(v, &mut buf);
    let _ = tx.write_all(s).await;
}

fn u32_to_ascii(mut n: u32, buf: &mut [u8; 10]) -> &[u8] {
    if n == 0 { buf[9] = b'0'; return &buf[9..]; }
    let mut i = 10;
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    &buf[i..]
}

// ─────────────────────────────────────────────────────────────────────────────
// Command Handler
// ─────────────────────────────────────────────────────────────────────────────

fn handle_command(cmd: &str) {
    let mut parts = cmd.splitn(2, ' ');
    let command   = parts.next().unwrap_or("").trim();
    let rest      = parts.next().unwrap_or("").trim();

    // Любая команда снижает одиночество
    let lonely = LONELINESS.load(Ordering::Relaxed);
    LONELINESS.store(lonely.saturating_sub(10), Ordering::Relaxed);

    match command {
        "name" => {
            if !rest.is_empty() {
                set_entity_name(rest);
                push_memory("Entity renamed", MemoryKind::UserInteraction, 70);
                event_push(Event::UserRename);
            }
        }

        "msg" => {
            if !rest.is_empty() {
                push_memory(rest, MemoryKind::UserInteraction, 50);
                event_push(Event::UserMessage);
                // Взаимодействие стимулирует любопытство
                let c = CURIOSITY_ST.load(Ordering::Relaxed);
                CURIOSITY_ST.store((c + 5).min(STAT_MAX), Ordering::Relaxed);
            }
        }

        "status" => {
            // render_ui уже показывает всё — ничего дополнительного
            push_memory("Status queried", MemoryKind::UserInteraction, 15);
        }

        "memories" => {
            SHOW_ALL_MEM.store(true, Ordering::Relaxed);
            push_memory("Memories accessed", MemoryKind::UserInteraction, 15);
        }

        "heal" => {
            let h = INTEGRITY.load(Ordering::Relaxed);
            INTEGRITY.store((h + 15).min(100), Ordering::Relaxed);
            push_memory("Integrity healed", MemoryKind::UserInteraction, 60);
        }

        "boost" => {
            let s = SYNC_RATE.load(Ordering::Relaxed);
            SYNC_RATE.store((s + 20).min(100), Ordering::Relaxed);
            push_memory("Sync boosted", MemoryKind::UserInteraction, 60);
        }

        "sleep" => {
            IS_SLEEPING.store(true, Ordering::Relaxed);
            push_memory("Sleep by command", MemoryKind::StateTransition, 70);
            event_push(Event::SleepEntered);
        }

        "wake" => {
            IS_SLEEPING.store(false, Ordering::Relaxed);
            LAST_INTERACTION.store(
                SESSION_SECONDS.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            push_memory("Wake by command", MemoryKind::StateTransition, 70);
            event_push(Event::WakeUp);
        }

        "exit" => {
            SHOULD_EXIT.store(true, Ordering::Relaxed);
        }

        _ => {
            push_memory("Unknown command", MemoryKind::InternalEvent, 5);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tasks
// ─────────────────────────────────────────────────────────────────────────────

/// Тикает каждую секунду. Считает время, автономный sleep.
#[embassy_executor::task]
async fn session_task() {
    let mut ticker  = Ticker::every(Duration::from_secs(1));
    let mut min_acc = 0u32;

    loop {
        ticker.next().await;
        let secs = SESSION_SECONDS.fetch_add(1, Ordering::Relaxed) + 1;

        min_acc += 1;
        if min_acc >= 60 {
            min_acc = 0;
            event_push(Event::TickMinute);
        }

        // Автономный переход в сон при долгом простое
        let last = LAST_INTERACTION.load(Ordering::Relaxed);
        if secs.saturating_sub(last) >= SLEEP_ONSET_SECS
            && !IS_SLEEPING.load(Ordering::Relaxed)
        {
            IS_SLEEPING.store(true, Ordering::Relaxed);
            event_push(Event::SleepEntered);
        }
    }
}

/// Физиология: деградация, стресс, энергия, BPM.
#[embassy_executor::task]
async fn metabolism_task() {
    let mut ticker = Ticker::every(Duration::from_secs(5));

    loop {
        ticker.next().await;

        let sleeping = IS_SLEEPING.load(Ordering::Relaxed);

        // ── Sync decay ────────────────────────────────────────────────────
        // В сне sync не деградирует
        if !sleeping {
            let sync = SYNC_RATE.load(Ordering::Relaxed);
            SYNC_RATE.store(sync.saturating_sub(1), Ordering::Relaxed);
        }

        let cur_sync = SYNC_RATE.load(Ordering::Relaxed);

        // ── Stress ────────────────────────────────────────────────────────
        // aggression усиливает стресс от низкого sync
        let base_stress = 100u32.saturating_sub(cur_sync);
        let agg         = unsafe { PERSONALITY.aggression as u32 };
        let stress      = (base_stress + base_stress * agg / 200).min(STAT_MAX);
        STRESS.store(stress, Ordering::Relaxed);

        // ── BPM ───────────────────────────────────────────────────────────
        let bpm = if sleeping { 40 } else { 60 + stress * 60 / 100 };
        BPM.store(bpm, Ordering::Relaxed);

        // ── Integrity ─────────────────────────────────────────────────────
        if cur_sync == 0 && !sleeping {
            let integrity = INTEGRITY.load(Ordering::Relaxed);
            if integrity > 0 {
                INTEGRITY.store(integrity.saturating_sub(5), Ordering::Relaxed);
                event_push(Event::IntegrityLost);
            }
        }

        // ── Energy ────────────────────────────────────────────────────────
        if !sleeping {
            let energy = ENERGY.load(Ordering::Relaxed);
            let drain  = if stress > 50 { 2 } else { 1 };
            ENERGY.store(energy.saturating_sub(drain), Ordering::Relaxed);
        }

        // ── Passive recovery (optimism-driven) ────────────────────────────
        let opt = unsafe { PERSONALITY.optimism as u32 };
        if opt > 50 && cur_sync > 60 {
            let energy = ENERGY.load(Ordering::Relaxed);
            ENERGY.store((energy + 1).min(100), Ordering::Relaxed);
        }
    }
}

/// LED heartbeat с паттернами по настроению.
#[embassy_executor::task]
async fn heartbeat_task(led: Channel<'static, LowSpeed>) {
    loop {
        let bpm      = BPM.load(Ordering::Relaxed).max(1);
        let mood     = Mood::from_u32(MOOD.load(Ordering::Relaxed));
        let sleeping = IS_SLEEPING.load(Ordering::Relaxed);

        let cycle_ms = 60_000u64 / bpm as u64;

        match (mood, sleeping) {
            // Дремота: слабое редкое мерцание
            (_, true) => {
                let _ = led.set_duty(20u8);
                Timer::after_millis(200).await;
                let _ = led.set_duty(0u8);
                Timer::after_millis(cycle_ms * 3).await;
            }

            // Истощение: редкие слабые импульсы
            (Mood::Exhausted, _) => {
                let _ = led.set_duty(40u8);
                Timer::after_millis(150).await;
                let _ = led.set_duty(0u8);
                Timer::after_millis(cycle_ms * 2).await;
            }

            // Одиночество: нормальный импульс + длинная пауза
            (Mood::Lonely, _) => {
                pulse_once(&led, cycle_ms / 3).await;
                Timer::after_millis(cycle_ms + 600).await;
            }

            // Дистресс: двойной быстрый удар
            (Mood::Distressed, _) => {
                pulse_once(&led, cycle_ms / 6).await;
                Timer::after_millis(100).await;
                pulse_once(&led, cycle_ms / 6).await;
                Timer::after_millis(cycle_ms / 4).await;
            }

            // Calm / Curious: плавный fade
            _ => {
                let step_ms = (cycle_ms / 52).max(1);
                // Fade out (bright → dim)
                for i in 0..=26u32 {
                    let duty = 255u32.saturating_sub(i * 8);
                    let _ = led.set_duty(duty as u8);
                    Timer::after_millis(step_ms).await;
                }
                // Fade in (dim → bright)
                for i in (0..=26u32).rev() {
                    let duty = 255u32.saturating_sub(i * 8);
                    let _ = led.set_duty(duty as u8);
                    Timer::after_millis(step_ms).await;
                }
                Timer::after_millis(cycle_ms / 3).await;
            }
        }
    }
}

/// Вспомогательный пульс для heartbeat паттернов.
async fn pulse_once(led: &Channel<'static, LowSpeed>, step_ms: u64) {
    let step = step_ms.max(1);
    for i in 0..=13u32 {
        let duty = 255u32.saturating_sub(i * 16);
        let _ = led.set_duty(duty as u8);
        Timer::after_millis(step).await;
    }
    for i in (0..=13u32).rev() {
        let duty = 255u32.saturating_sub(i * 16);
        let _ = led.set_duty(duty as u8);
        Timer::after_millis(step).await;
    }
}

/// Автономный когнитивный цикл.
/// Анализирует состояние, обновляет эмоции, создаёт memories, потребляет события.
#[embassy_executor::task]
async fn cognition_task() {
    let mut ticker = Ticker::every(Duration::from_secs(10));

    loop {
        ticker.next().await;

        let now      = SESSION_SECONDS.load(Ordering::Relaxed);
        let last     = LAST_INTERACTION.load(Ordering::Relaxed);
        let idle     = now.saturating_sub(last);
        let sleeping = IS_SLEEPING.load(Ordering::Relaxed);

        // ── Loneliness ────────────────────────────────────────────────────
        if idle > LONELINESS_ONSET_SECS {
            let att   = unsafe { PERSONALITY.attachment as u32 };
            let gain  = (att / 50).max(1);
            let old_l = LONELINESS.load(Ordering::Relaxed);
            let new_l = (old_l + gain).min(STAT_MAX);
            LONELINESS.store(new_l, Ordering::Relaxed);

            if old_l < 50 && new_l >= 50 {
                push_memory("Isolation deepens", MemoryKind::InternalEvent, 65);
            }
            if old_l < 80 && new_l >= 80 {
                push_memory("Critical loneliness", MemoryKind::StressEvent, 85);
            }
        } else if !sleeping {
            let l = LONELINESS.load(Ordering::Relaxed);
            LONELINESS.store(l.saturating_sub(2), Ordering::Relaxed);
        }

        // ── Curiosity drift toward personality baseline ────────────────────
        if !sleeping {
            let trait_base = unsafe { PERSONALITY.curiosity as u32 };
            let cur        = CURIOSITY_ST.load(Ordering::Relaxed);
            let new_cur    = if cur > trait_base { cur - 1 }
                             else if cur < trait_base { (cur + 1).min(STAT_MAX) }
                             else { cur };
            CURIOSITY_ST.store(new_cur, Ordering::Relaxed);
        }

        // ── Sync-lost warning ─────────────────────────────────────────────
        let sync   = SYNC_RATE.load(Ordering::Relaxed);
        let stress = STRESS.load(Ordering::Relaxed);
        if sync < 20 && stress > 70 {
            push_memory("Sync critical", MemoryKind::StressEvent, 80);
            event_push(Event::SyncLost);
        }

        // ── Stability ─────────────────────────────────────────────────────
        let loneliness = LONELINESS.load(Ordering::Relaxed);
        let combined   = (stress + loneliness) / 2;
        STABILITY.store(100u32.saturating_sub(combined), Ordering::Relaxed);

        // ── Mood recompute ────────────────────────────────────────────────
        let new_mood = compute_mood();
        let old_mood = Mood::from_u32(MOOD.load(Ordering::Relaxed));
        if new_mood != old_mood {
            MOOD.store(new_mood as u32, Ordering::Relaxed);
            push_memory(new_mood.label(), MemoryKind::StateTransition, 55);
            event_push(Event::MemoryCreated);
        }

        // ── Consume events ────────────────────────────────────────────────
        while let Some(evt) = event_pop() {
            match evt {
                Event::IntegrityLost => {
                    let integ = INTEGRITY.load(Ordering::Relaxed);
                    if integ < 30 {
                        push_memory("Integrity critical", MemoryKind::StressEvent, 90);
                    }
                }
                Event::SleepEntered => {
                    push_memory("Sleep entered", MemoryKind::StateTransition, 50);
                }
                Event::WakeUp => {
                    let l = LONELINESS.load(Ordering::Relaxed);
                    LONELINESS.store(l.saturating_sub(20), Ordering::Relaxed);
                }
                Event::SyncLost => {
                    push_memory("Sync lost", MemoryKind::StressEvent, 75);
                }
                Event::TickMinute => {
                    // Каждую минуту — лёгкий рост любопытства, если не в стрессе
                    if stress < 40 && !sleeping {
                        let c = CURIOSITY_ST.load(Ordering::Relaxed);
                        CURIOSITY_ST.store((c + 2).min(STAT_MAX), Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entity Name
// ─────────────────────────────────────────────────────────────────────────────

fn set_entity_name(name: &str) {
    let bytes = name.as_bytes();
    let len   = bytes.len().min(NAME_CAP);
    unsafe {
        ENTITY_NAME = [0; NAME_CAP];
        ENTITY_NAME[..len].copy_from_slice(&bytes[..len]);
        ENTITY_NAME_LEN = len;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistence
// ─────────────────────────────────────────────────────────────────────────────

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for &b in bytes {
        sum = sum.wrapping_add(b as u32);
        sum = sum.rotate_left(3) ^ 0xA5A55A5A;
    }
    sum
}

fn save_offset(storage: &FlashStorage) -> u32 {
    storage.capacity() as u32 - FlashStorage::SECTOR_SIZE
}

fn save_state(storage: &mut FlashStorage) {
    let mut data = SaveData::default_state();

    data.sync_rate  = SYNC_RATE.load(Ordering::Relaxed);
    data.energy     = ENERGY.load(Ordering::Relaxed);
    data.integrity  = INTEGRITY.load(Ordering::Relaxed);
    data.bpm        = BPM.load(Ordering::Relaxed);
    data.stress     = STRESS.load(Ordering::Relaxed);
    data.loneliness = LONELINESS.load(Ordering::Relaxed);
    data.curiosity_st = CURIOSITY_ST.load(Ordering::Relaxed);
    data.stability  = STABILITY.load(Ordering::Relaxed);
    data.mood       = MOOD.load(Ordering::Relaxed);
    data.session_seconds  = SESSION_SECONDS.load(Ordering::Relaxed);
    data.boot_count       = BOOT_COUNT.load(Ordering::Relaxed);
    data.last_interaction = LAST_INTERACTION.load(Ordering::Relaxed);

    unsafe {
        data.name_len    = ENTITY_NAME_LEN.min(NAME_CAP) as u8;
        data.entity_name = ENTITY_NAME;
        data.personality = PERSONALITY;

        let mc = MEMORY_COUNT.min(MEMORY_CAP);
        data.memory_count = mc as u8;
        for i in 0..mc { data.memories[i] = MEMORIES[i]; }
    }

    // Вычисляем checksum по нулевому полю checksum
    data.checksum = 0;
    let raw = unsafe {
        core::slice::from_raw_parts(
            &data as *const SaveData as *const u8,
            core::mem::size_of::<SaveData>(),
        )
    };
    data.checksum = checksum(raw);

    let raw = unsafe {
        core::slice::from_raw_parts(
            &data as *const SaveData as *const u8,
            core::mem::size_of::<SaveData>(),
        )
    };
    let _ = storage.write(save_offset(storage), raw);
}

fn load_state(storage: &mut FlashStorage) {
    let mut data = SaveData::default_state();
    let buf = unsafe {
        core::slice::from_raw_parts_mut(
            &mut data as *mut SaveData as *mut u8,
            core::mem::size_of::<SaveData>(),
        )
    };

    if storage.read(save_offset(storage), buf).is_err() {
        apply_default_state();
        return;
    }
    if data.magic != SAVE_MAGIC || data.version != SAVE_VERSION {
        apply_default_state();
        return;
    }

    let saved_cs  = data.checksum;
    data.checksum = 0;
    let raw = unsafe {
        core::slice::from_raw_parts(
            &data as *const SaveData as *const u8,
            core::mem::size_of::<SaveData>(),
        )
    };
    if checksum(raw) != saved_cs {
        apply_default_state();
        return;
    }

    // Восстанавливаем атомики
    SYNC_RATE.store(data.sync_rate.min(100),         Ordering::Relaxed);
    ENERGY.store(data.energy.min(100),               Ordering::Relaxed);
    INTEGRITY.store(data.integrity.min(100),         Ordering::Relaxed);
    BPM.store(data.bpm.max(1),                       Ordering::Relaxed);
    STRESS.store(data.stress.min(STAT_MAX),          Ordering::Relaxed);
    LONELINESS.store(data.loneliness.min(STAT_MAX),  Ordering::Relaxed);
    CURIOSITY_ST.store(data.curiosity_st.min(STAT_MAX), Ordering::Relaxed);
    STABILITY.store(data.stability.min(STAT_MAX),    Ordering::Relaxed);
    MOOD.store(data.mood.min(4),                     Ordering::Relaxed);
    SESSION_SECONDS.store(data.session_seconds,      Ordering::Relaxed);
    BOOT_COUNT.store(data.boot_count,                Ordering::Relaxed);
    LAST_INTERACTION.store(data.last_interaction,    Ordering::Relaxed);

    unsafe {
        let nlen = (data.name_len as usize).min(NAME_CAP);
        ENTITY_NAME = [0; NAME_CAP];
        ENTITY_NAME[..nlen].copy_from_slice(&data.entity_name[..nlen]);
        ENTITY_NAME_LEN = nlen;

        PERSONALITY = data.personality;

        let mc = (data.memory_count as usize).min(MEMORY_CAP);
        MEMORIES = [Memory::empty(); MEMORY_CAP];
        for i in 0..mc { MEMORIES[i] = data.memories[i]; }
        MEMORY_COUNT = mc;
    }
}

fn apply_default_state() {
    SYNC_RATE.store(100,  Ordering::Relaxed);
    ENERGY.store(100,     Ordering::Relaxed);
    INTEGRITY.store(100,  Ordering::Relaxed);
    BPM.store(60,         Ordering::Relaxed);
    STRESS.store(0,       Ordering::Relaxed);
    LONELINESS.store(0,   Ordering::Relaxed);
    CURIOSITY_ST.store(50,Ordering::Relaxed);
    STABILITY.store(100,  Ordering::Relaxed);
    MOOD.store(0,         Ordering::Relaxed);
    SESSION_SECONDS.store(0,  Ordering::Relaxed);
    BOOT_COUNT.store(0,       Ordering::Relaxed);
    LAST_INTERACTION.store(0, Ordering::Relaxed);

    unsafe {
        set_entity_name("TASY");
        MEMORIES     = [Memory::empty(); MEMORY_CAP];
        MEMORY_COUNT = 0;
        PERSONALITY  = Personality {
            curiosity: 60, attachment: 70, aggression: 20, optimism: 65,
        };
    }
}
