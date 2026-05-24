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

const NAME_CAP: usize = 32;
const MESSAGE_CAP: usize = 16;
const MESSAGE_TEXT_CAP: usize = 128;

const SAVE_MAGIC: u32 = 0x54535959;
const SAVE_VERSION: u32 = 1;

static SYNC_RATE: AtomicU32 = AtomicU32::new(100);
static ENERGY: AtomicU32 = AtomicU32::new(100);
static INTEGRITY: AtomicU32 = AtomicU32::new(100);
static BPM: AtomicU32 = AtomicU32::new(60);

static SESSION_SECONDS: AtomicU32 = AtomicU32::new(0);
static BOOT_COUNT: AtomicU32 = AtomicU32::new(0);

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Copy, Clone)]
struct Message {
    session_sec: u32,
    len: u8,
    _pad: [u8; 3],
    text: [u8; MESSAGE_TEXT_CAP],
}

impl Message {
    const fn empty() -> Self {
        Self {
            session_sec: 0,
            len: 0,
            _pad: [0; 3],
            text: [0; MESSAGE_TEXT_CAP],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct SaveData {
    magic: u32,
    version: u32,
    checksum: u32,

    sync_rate: u32,
    energy: u32,
    integrity: u32,
    bpm: u32,

    session_seconds: u32,
    boot_count: u32,

    name_len: u8,
    message_count: u8,
    _pad: [u8; 2],

    entity_name: [u8; NAME_CAP],
    messages: [Message; MESSAGE_CAP],
}

impl SaveData {
    const fn default_state() -> Self {
        Self {
            magic: SAVE_MAGIC,
            version: SAVE_VERSION,
            checksum: 0,

            sync_rate: 100,
            energy: 100,
            integrity: 100,
            bpm: 60,

            session_seconds: 0,
            boot_count: 0,

            name_len: 0,
            message_count: 0,
            _pad: [0; 2],

            entity_name: [0; NAME_CAP],
            messages: [Message::empty(); MESSAGE_CAP],
        }
    }
}

static mut ENTITY_NAME: [u8; NAME_CAP] = [0; NAME_CAP];
static mut ENTITY_NAME_LEN: usize = 0;

static mut MESSAGES: [Message; MESSAGE_CAP] =
    [Message::empty(); MESSAGE_CAP];

static mut MESSAGE_COUNT: usize = 0;

static mut TIMER_STORAGE:
    MaybeUninit<timer::Timer<'static, LowSpeed>> =
    MaybeUninit::uninit();

#[main]
async fn main(spawner: Spawner) -> ! {
    let peripherals =
        esp_hal::init(esp_hal::Config::default());

    let timer_group =
        esp_hal::timer::timg::TimerGroup::new(
            peripherals.TIMG0,
        );

    esp_hal_embassy::init(timer_group.timer0);

    let _io = Io::new(peripherals.IO_MUX);

    let mut ledc = Ledc::new(peripherals.LEDC);

    ledc.set_global_slow_clock(
        esp_hal::ledc::LSGlobalClkSource::APBClk,
    );

    let timer_raw =
        ledc.timer::<LowSpeed>(timer::Number::Timer0);

    let timer = unsafe {
        let static_ptr =
            core::ptr::addr_of_mut!(TIMER_STORAGE);

        let raw_ptr = (*static_ptr).as_mut_ptr();

        raw_ptr.write(timer_raw);

        &mut *raw_ptr
    };

    timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: LSClockSource::APBClk,
            frequency: 5.kHz(),
        })
        .unwrap();

    let mut led_channel =
        ledc.channel(
            channel::Number::Channel0,
            peripherals.GPIO8,
        );

    led_channel
        .configure(channel::config::Config {
            timer,
            duty_pct: 0,
            pin_config:
                channel::config::PinConfig::PushPull,
        })
        .unwrap();

    let usb_serial =
        UsbSerialJtag::new(peripherals.USB_DEVICE)
            .into_async();

    let (mut rx, mut tx) = usb_serial.split();

    let mut flash = FlashStorage::new();

    load_state(&mut flash);

    unsafe {
        if ENTITY_NAME_LEN == 0 {
            set_entity_name("TASY");
        }
    }

    BOOT_COUNT.fetch_add(1, Ordering::Relaxed);

    push_message(
        "System boot completed",
        SESSION_SECONDS.load(Ordering::Relaxed),
    );

    save_state(&mut flash);

    spawner.spawn(session_task()).unwrap();
    spawner.spawn(metabolism_task()).unwrap();
    spawner.spawn(heartbeat_task(led_channel)).unwrap();

    render_ui(&mut tx).await;

    let _ =
        tx.write_all(b"\r\nEnter Command > ")
            .await;

    let mut input = [0u8; 128];
    let mut pos = 0usize;

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            let _ = tx
                .write_all(
                    b"\r\n[TASY] shutting down...\r\n",
                )
                .await;

            save_state(&mut flash);

            loop {
                Timer::after_secs(1).await;
            }
        }

        let mut byte = [0u8; 1];

        let got_input =
            embassy_time::with_timeout(
                Duration::from_millis(100),
                rx.read_exact(&mut byte),
            )
            .await
            .is_ok();

        if got_input {
            match byte[0] {
                b'\r' | b'\n' => {
                    let _ =
                        tx.write_all(b"\r\n").await;

                    if pos > 0 {
                        let cmd =
                            core::str::from_utf8(
                                &input[..pos],
                            )
                            .unwrap_or("");

                        handle_command(cmd);

                        save_state(&mut flash);

                        pos = 0;

                        render_ui(&mut tx).await;

                        let _ = tx
                            .write_all(
                                b"\r\nEnter Command > ",
                            )
                            .await;
                    }
                }

                8 | 127 => {
                    if pos > 0 {
                        pos -= 1;

                        let _ = tx
                            .write_all(b"\x08 \x08")
                            .await;
                    }
                }

                b => {
                    if pos < input.len() - 1 {
                        input[pos] = b;
                        pos += 1;

                        let _ =
                            tx.write_all(&[b]).await;
                    }
                }
            }
        }

        Timer::after_millis(10).await;
    }
}

async fn render_ui<W>(tx: &mut W)
where
    W: Write,
{
    let sync =
        SYNC_RATE.load(Ordering::Relaxed);

    let energy =
        ENERGY.load(Ordering::Relaxed);

    let integrity =
        INTEGRITY.load(Ordering::Relaxed);

    let bpm = BPM.load(Ordering::Relaxed);

    let session_seconds =
        SESSION_SECONDS.load(Ordering::Relaxed);

    let boot_count =
        BOOT_COUNT.load(Ordering::Relaxed);

    let (name_bytes, name_len, message_count) =
        unsafe {
            (
                ENTITY_NAME,
                ENTITY_NAME_LEN.min(NAME_CAP),
                MESSAGE_COUNT.min(MESSAGE_CAP),
            )
        };

    let name =
        core::str::from_utf8(
            &name_bytes[..name_len],
        )
        .unwrap_or("TASY");

    let _ =
        tx.write_all(b"\x1B[2J\x1B[H").await;

    let _ = tx
        .write_all(
            b"==================================================\r\n",
        )
        .await;

    let _ = tx
        .write_all(
            b"           [ TASY v1 - AUTONOMOUS CORE ]          \r\n",
        )
        .await;

    let _ = tx
        .write_all(
            b"==================================================\r\n",
        )
        .await;

    let _ =
        tx.write_all(b"  Entity:        ")
            .await;

    let _ = tx.write_all(name.as_bytes()).await;

    let _ = tx.write_all(b"\r\n").await;

    let _ =
        tx.write_all(b"  Sync Rate:     ")
            .await;

    write_u32(tx, sync).await;

    let _ = tx.write_all(b" %\r\n").await;

    let _ =
        tx.write_all(b"  Core Energy:   ")
            .await;

    write_u32(tx, energy).await;

    let _ = tx.write_all(b" %\r\n").await;

    let _ =
        tx.write_all(b"  Integrity:     ")
            .await;

    write_u32(tx, integrity).await;

    let _ = tx.write_all(b" %\r\n").await;

    let _ = tx
        .write_all(
            b"--------------------------------------------------\r\n",
        )
        .await;

    let _ =
        tx.write_all(b"  Heart Rate:    ")
            .await;

    write_u32(tx, bpm).await;

    let _ = tx.write_all(b" BPM\r\n").await;

    let _ =
        tx.write_all(b"  Boot Count:    ")
            .await;

    write_u32(tx, boot_count).await;

    let _ = tx.write_all(b"\r\n").await;

    let _ =
        tx.write_all(b"  Session Time:  ")
            .await;

    write_u32(tx, session_seconds).await;

    let _ = tx.write_all(b" sec\r\n").await;

    let _ = tx
        .write_all(
            b"==================================================\r\n",
        )
        .await;

    let _ =
        tx.write_all(b"  [ Messages ]\r\n")
            .await;

    for i in 0..message_count {
        let msg = unsafe { MESSAGES[i] };

        if msg.len == 0 {
            continue;
        }

        let text =
            core::str::from_utf8(
                &msg.text[..msg.len as usize],
            )
            .unwrap_or("<invalid>");

        let _ = tx.write_all(b"  - [").await;

        write_u32(tx, msg.session_sec).await;

        let _ = tx.write_all(b"s] ").await;

        let _ = tx.write_all(text.as_bytes()).await;

        let _ = tx.write_all(b"\r\n").await;
    }

    let _ = tx
        .write_all(
            b"==================================================\r\n",
        )
        .await;

    let _ =
        tx.write_all(b"  Commands:\r\n")
            .await;

    let _ =
        tx.write_all(b"  name <new_name>\r\n")
            .await;

    let _ =
        tx.write_all(b"  msg <text>\r\n")
            .await;

    let _ =
        tx.write_all(b"  heal\r\n")
            .await;

    let _ =
        tx.write_all(b"  boost\r\n")
            .await;

    let _ =
        tx.write_all(b"  exit\r\n")
            .await;
}

async fn write_u32<W>(
    tx: &mut W,
    value: u32,
) where
    W: Write,
{
    let mut buf = [0u8; 10];

    let s = u32_to_ascii(value, &mut buf);

    let _ = tx.write_all(s).await;
}

fn u32_to_ascii<'a>(
    mut n: u32,
    buf: &'a mut [u8; 10],
) -> &'a [u8] {
    if n == 0 {
        buf[9] = b'0';
        return &buf[9..10];
    }

    let mut i = 10;

    while n > 0 {
        i -= 1;

        buf[i] =
            b'0' + (n % 10) as u8;

        n /= 10;
    }

    &buf[i..10]
}

fn handle_command(cmd: &str) {
    let mut parts =
        cmd.splitn(2, ' ');

    let command =
        parts.next().unwrap_or("").trim();

    let rest =
        parts.next().unwrap_or("").trim();

    match command {
        "name" => {
            set_entity_name(rest);

            push_message(
                "Entity name updated",
                SESSION_SECONDS.load(
                    Ordering::Relaxed,
                ),
            );
        }

        "msg" => {
            push_message(
                rest,
                SESSION_SECONDS.load(
                    Ordering::Relaxed,
                ),
            );
        }

        "heal" => {
            let h =
                INTEGRITY.load(Ordering::Relaxed);

            INTEGRITY.store(
                (h + 10).min(100),
                Ordering::Relaxed,
            );

            push_message(
                "Integrity healed",
                SESSION_SECONDS.load(
                    Ordering::Relaxed,
                ),
            );
        }

        "boost" => {
            let s =
                SYNC_RATE.load(Ordering::Relaxed);

            SYNC_RATE.store(
                (s + 15).min(100),
                Ordering::Relaxed,
            );

            push_message(
                "Sync boosted",
                SESSION_SECONDS.load(
                    Ordering::Relaxed,
                ),
            );
        }

        "exit" => {
            SHOULD_EXIT.store(
                true,
                Ordering::Relaxed,
            );
        }

        _ => {
            push_message(
                "Unknown command",
                SESSION_SECONDS.load(
                    Ordering::Relaxed,
                ),
            );
        }
    }
}

#[embassy_executor::task]
async fn session_task() {
    let mut ticker =
        Ticker::every(Duration::from_secs(1));

    loop {
        ticker.next().await;

        SESSION_SECONDS.fetch_add(
            1,
            Ordering::Relaxed,
        );
    }
}

#[embassy_executor::task]
async fn metabolism_task() {
    let mut ticker =
        Ticker::every(Duration::from_secs(4));

    loop {
        ticker.next().await;

        let sync =
            SYNC_RATE.load(Ordering::Relaxed);

        if sync > 0 {
            SYNC_RATE.fetch_sub(
                1,
                Ordering::Relaxed,
            );
        }

        let current_sync =
            SYNC_RATE.load(Ordering::Relaxed);

        let stress =
            100 - current_sync;

        let calculated_bpm =
            60 + (stress * 60 / 100);

        BPM.store(
            calculated_bpm,
            Ordering::Relaxed,
        );

        if current_sync == 0 {
            let integrity =
                INTEGRITY.load(
                    Ordering::Relaxed,
                );

            if integrity > 0 {
                INTEGRITY.fetch_sub(
                    5,
                    Ordering::Relaxed,
                );
            }
        }
    }
}

#[embassy_executor::task]
async fn heartbeat_task(
    led: Channel<'static, LowSpeed>,
) {
    loop {
        let bpm =
            BPM.load(Ordering::Relaxed)
                .max(1);

        let total_cycle_ms =
            (60_000 / bpm) as u64;

        let step_delay =
            Duration::from_millis(
                (total_cycle_ms / 50).max(1),
            );

        for duty in (0..=200).step_by(8) {
            let _ =
                led.set_duty(255 - duty);

            Timer::after(step_delay).await;
        }

        for duty in (0..=200)
            .step_by(8)
            .rev()
        {
            let _ =
                led.set_duty(255 - duty);

            Timer::after(step_delay).await;
        }

        Timer::after(
            Duration::from_millis(
                (total_cycle_ms / 3).max(1),
            ),
        )
        .await;
    }
}

fn set_entity_name(name: &str) {
    let bytes = name.as_bytes();

    let len =
        bytes.len().min(NAME_CAP);

    unsafe {
        ENTITY_NAME = [0; NAME_CAP];

        ENTITY_NAME[..len]
            .copy_from_slice(
                &bytes[..len],
            );

        ENTITY_NAME_LEN = len;
    }
}

fn push_message(
    text: &str,
    session_sec: u32,
) {
    let bytes = text.as_bytes();

    let len = bytes
        .len()
        .min(MESSAGE_TEXT_CAP);

    unsafe {
        if MESSAGE_COUNT >= MESSAGE_CAP {
            for i in 1..MESSAGE_CAP {
                MESSAGES[i - 1] =
                    MESSAGES[i];
            }

            MESSAGE_COUNT =
                MESSAGE_CAP - 1;
        }

        let mut msg =
            Message::empty();

        msg.session_sec =
            session_sec;

        msg.len = len as u8;

        msg.text[..len]
            .copy_from_slice(
                &bytes[..len],
            );

        MESSAGES[MESSAGE_COUNT] = msg;

        MESSAGE_COUNT += 1;
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;

    for &b in bytes {
        sum = sum
            .wrapping_add(b as u32);

        sum =
            sum.rotate_left(3)
                ^ 0xA5A55A5A;
    }

    sum
}

fn save_offset(
    storage: &FlashStorage,
) -> u32 {
    storage.capacity() as u32
        - FlashStorage::SECTOR_SIZE
}

fn save_state(
    storage: &mut FlashStorage,
) {
    let mut data =
        SaveData::default_state();

    data.sync_rate =
        SYNC_RATE.load(Ordering::Relaxed);

    data.energy =
        ENERGY.load(Ordering::Relaxed);

    data.integrity =
        INTEGRITY.load(Ordering::Relaxed);

    data.bpm =
        BPM.load(Ordering::Relaxed);

    data.session_seconds =
        SESSION_SECONDS.load(
            Ordering::Relaxed,
        );

    data.boot_count =
        BOOT_COUNT.load(Ordering::Relaxed);

    unsafe {
        data.name_len =
            ENTITY_NAME_LEN
                .min(NAME_CAP)
                as u8;

        data.entity_name =
            ENTITY_NAME;

        data.message_count =
            MESSAGE_COUNT
                .min(MESSAGE_CAP)
                as u8;

        data.messages =
            MESSAGES;
    }

    data.checksum = 0;

    let raw = unsafe {
        core::slice::from_raw_parts(
            (&data
                as *const SaveData)
                as *const u8,
            core::mem::size_of::<
                SaveData,
            >(),
        )
    };

    data.checksum =
        checksum(raw);

    let raw = unsafe {
        core::slice::from_raw_parts(
            (&data
                as *const SaveData)
                as *const u8,
            core::mem::size_of::<
                SaveData,
            >(),
        )
    };

    let addr =
        save_offset(storage);

    let _ =
        storage.write(addr, raw);
}

fn load_state(
    storage: &mut FlashStorage,
) {
    let mut data =
        SaveData::default_state();

    let buf = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut data
                as *mut SaveData)
                as *mut u8,
            core::mem::size_of::<
                SaveData,
            >(),
        )
    };

    if storage
        .read(
            save_offset(storage),
            buf,
        )
        .is_err()
    {
        apply_default_state();
        return;
    }

    if data.magic != SAVE_MAGIC {
        apply_default_state();
        return;
    }

    if data.version != SAVE_VERSION {
        apply_default_state();
        return;
    }

    let saved_checksum =
        data.checksum;

    data.checksum = 0;

    let raw = unsafe {
        core::slice::from_raw_parts(
            (&data
                as *const SaveData)
                as *const u8,
            core::mem::size_of::<
                SaveData,
            >(),
        )
    };

    if checksum(raw)
        != saved_checksum
    {
        apply_default_state();
        return;
    }

    SYNC_RATE.store(
        data.sync_rate.min(100),
        Ordering::Relaxed,
    );

    ENERGY.store(
        data.energy.min(100),
        Ordering::Relaxed,
    );

    INTEGRITY.store(
        data.integrity.min(100),
        Ordering::Relaxed,
    );

    BPM.store(
        data.bpm.max(1),
        Ordering::Relaxed,
    );

    SESSION_SECONDS.store(
        data.session_seconds,
        Ordering::Relaxed,
    );

    BOOT_COUNT.store(
        data.boot_count,
        Ordering::Relaxed,
    );

    unsafe {
        ENTITY_NAME =
            [0; NAME_CAP];

        let nlen =
            (data.name_len
                as usize)
                .min(NAME_CAP);

        ENTITY_NAME[..nlen]
            .copy_from_slice(
                &data.entity_name
                    [..nlen],
            );

        ENTITY_NAME_LEN =
            nlen;

        MESSAGES =
            [Message::empty();
                MESSAGE_CAP];

        let mcount =
            (data.message_count
                as usize)
                .min(MESSAGE_CAP);

        for i in 0..mcount {
            MESSAGES[i] =
                data.messages[i];
        }

        MESSAGE_COUNT =
            mcount;
    }
}

fn apply_default_state() {
    SYNC_RATE.store(
        100,
        Ordering::Relaxed,
    );

    ENERGY.store(
        100,
        Ordering::Relaxed,
    );

    INTEGRITY.store(
        100,
        Ordering::Relaxed,
    );

    BPM.store(
        60,
        Ordering::Relaxed,
    );

    SESSION_SECONDS.store(
        0,
        Ordering::Relaxed,
    );

    BOOT_COUNT.store(
        0,
        Ordering::Relaxed,
    );

    unsafe {
        set_entity_name("TASY");

        MESSAGES =
            [Message::empty();
                MESSAGE_CAP];

        MESSAGE_COUNT = 0;
    }
}
