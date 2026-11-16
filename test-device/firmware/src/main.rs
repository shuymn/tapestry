//! The test input device's firmware (docs/test-device.md): one composite
//! USB device on the Pico 2 with a keyboard (interface 0), a game pad
//! (interface 1) and a control link (CDC ACM, interfaces 2-3). Every input
//! is synthesized from commands and patterns received over the link.
//!
//! All work runs as futures joined in one executor task, so a future that
//! stops yielding stops the watchdog's feeder too.

#![no_std]
#![no_main]

mod device;

use core::cell::RefCell;
use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_futures::join::{join, join5};
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_rp::watchdog::{ResetReason, Watchdog};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::cdc_acm::{
    CdcAcmClass, ControlChanged, Receiver, Sender, State as CdcState,
};
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidState,
};
use embassy_usb::{Builder, Config, Handler};
use static_cell::ConstStaticCell;
use test_device::boot::{self, Action, Cause};
use test_device::faults::Fault;
use test_device::lease::Reason;
use test_device::mode::Mode;
use test_device::protocol::{self, Answer, Command, Error, Hello};
use test_device::report::{KEYBOARD_DESCRIPTOR, KEYBOARD_REPORT, PAD_DESCRIPTOR, PAD_REPORT};
use test_device::schedule::Refused;
use test_device::trace::{Body, Ep};

use crate::device::{KEYBOARD_EP, Next, PAD_EP, State, now};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

static STATE: ConstStaticCell<RefCell<State>> = ConstStaticCell::new(RefCell::new(State::new()));

/// The watchdog's timeout, and how often the feeder feeds it.
const WATCHDOG: Duration = Duration::from_secs(2);
const SLOW_WATCHDOG: Duration = Duration::from_secs(8);
const FEED_EVERY: Duration = Duration::from_millis(250);
/// How often the lease's own timer checks the deadline.
const LEASE_TICK: Duration = Duration::from_micros(500);
/// The longest `detach`.
const MAX_DETACH_MS: u32 = 10_000;

type Line = heapless::String<{ protocol::MAX_LINE }>;
type Answers = Channel<NoopRawMutex, Line, 8>;

/// Reboots into the boot ROM's drive, clearing the boot counter and the
/// boot-hang fault first so the next firmware starts clean.
fn enter_bootsel(watchdog: &mut Watchdog) -> ! {
    watchdog.set_scratch(1, boot::word(0));
    watchdog.set_scratch(2, 0);
    // REBOOT_TYPE_BOOTSEL | NO_RETURN_ON_SUCCESS, after 10 ms; no activity
    // pin; the drive and PICOBOOT both kept.
    embassy_rp::rom_data::reboot(0x0002 | 0x0100, 10, 0, 0);
    loop {
        cortex_m::asm::wfi();
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut watchdog = Watchdog::new(p.WATCHDOG);
    let cause = match watchdog.reset_reason() {
        Some(ResetReason::TimedOut) => Cause::Watchdog,
        Some(ResetReason::Forced) => Cause::Forced,
        None => Cause::PowerOn,
    };
    watchdog.start(WATCHDOG);
    let (count, action) = boot::on_boot(cause, watchdog.get_scratch(1));
    watchdog.set_scratch(1, boot::word(count));
    if action == Action::Bootsel {
        enter_bootsel(&mut watchdog);
    }
    #[cfg(feature = "debug-faults")]
    device::boot_hang(&mut watchdog, cause);
    let mode = Mode::from_scratch(watchdog.get_scratch(0));

    let state: &'static RefCell<State> = STATE.take();
    {
        let mut s = state.borrow_mut();
        s.mode = mode;
        s.boot = cause;
        s.count = count;
    }
    let watchdog = RefCell::new(watchdog);

    let driver = Driver::new(p.USB, Irqs);
    let mut config = Config::new(test_device::VID, test_device::PID);
    config.manufacturer = Some("tapestry");
    config.product = Some("tapestry test input device");
    config.serial_number = None;
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.device_release = mode.bcd_device();
    // The CDC function needs interface association descriptors.
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut msos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut keyboard_state = HidState::new();
    let mut pad_state = HidState::new();
    let mut cdc_state = CdcState::new();
    let mut bus = Bus { state };

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );
    builder.handler(&mut bus);
    let hid_config = |report_descriptor| HidConfig {
        report_descriptor,
        request_handler: None,
        poll_ms: mode.rate.interval_ms(),
        max_packet_size: 64,
        hid_subclass: HidSubclass::No,
        hid_boot_protocol: HidBootProtocol::None,
    };
    let mut keyboard = HidWriter::<_, KEYBOARD_REPORT>::new(
        &mut builder,
        &mut keyboard_state,
        hid_config(KEYBOARD_DESCRIPTOR),
    );
    let mut pad =
        HidWriter::<_, PAD_REPORT>::new(&mut builder, &mut pad_state, hid_config(PAD_DESCRIPTOR));
    let cdc = CdcAcmClass::new(&mut builder, &mut cdc_state, 64);
    let (mut tx, mut rx, control) = cdc.split_with_control();
    let mut usb = builder.build();
    let answers = Answers::new();

    let keyboard_loop = async {
        loop {
            keyboard.ready().await;
            wait_for_send(state, Ep::Keyboard).await;
            let report = state.borrow_mut().keyboard_report();
            if keyboard.write(&report).await.is_ok() {
                let neutral = report.iter().all(|&b| b == 0);
                state
                    .borrow_mut()
                    .handed(Ep::Keyboard, KEYBOARD_EP, neutral, KEYBOARD_REPORT);
            }
            complete(state, Ep::Keyboard, KEYBOARD_EP).await;
        }
    };
    let pad_loop = async {
        loop {
            pad.ready().await;
            wait_for_send(state, Ep::Pad).await;
            let report = state.borrow_mut().pad_report();
            if pad.write(&report).await.is_ok() {
                let neutral = report.get(..2) == Some(&[0, 0]);
                state
                    .borrow_mut()
                    .handed(Ep::Pad, PAD_EP, neutral, PAD_REPORT);
            }
            complete(state, Ep::Pad, PAD_EP).await;
            let skip = state.borrow().fault(Fault::EveryOtherPoll).is_some();
            if skip {
                let interval = state.borrow().mode.rate.interval_us();
                Timer::after_micros(u64::from(interval)).await;
            }
        }
    };
    let link_in = receive(&mut rx, &control, state, &answers, &watchdog);
    let link_out = send(&mut tx, state, &answers);
    let lease_timer = async {
        loop {
            Timer::after(LEASE_TICK).await;
            let mut s = state.borrow_mut();
            let t = now();
            if s.lease.expired(t) {
                s.disarm(Reason::Expired);
            }
            if !s.counter_cleared && boot::healthy(t, s.hello_answered) {
                watchdog.borrow_mut().set_scratch(1, boot::word(0));
                s.counter_cleared = true;
            }
        }
    };
    let feeder = async {
        loop {
            let slow = state.borrow().fault(Fault::SlowWatchdog).is_some();
            watchdog
                .borrow_mut()
                .feed(if slow { SLOW_WATCHDOG } else { WATCHDOG });
            Timer::after(FEED_EVERY).await;
        }
    };

    join(
        join5(usb.run(), keyboard_loop, pad_loop, link_in, link_out),
        join(lease_timer, feeder),
    )
    .await;
}

/// The bus events that disarm, and the configuration that identifies the
/// endpoints.
struct Bus {
    state: &'static RefCell<State>,
}

impl Handler for Bus {
    fn reset(&mut self) {
        self.state.borrow_mut().disarm(Reason::Reset);
    }

    fn suspended(&mut self, suspended: bool) {
        if suspended {
            self.state.borrow_mut().disarm(Reason::Suspended);
        }
    }

    fn configured(&mut self, configured: bool) {
        if configured {
            // One report on each endpoint, the current (neutral) state:
            // its buffer's LENGTH identifies the endpoint.
            let mut s = self.state.borrow_mut();
            for ep in [Ep::Keyboard, Ep::Pad] {
                s.ep(ep).dirty = true;
                device::kick(ep).signal(());
            }
        }
    }
}

/// Waits until `ep` has a report to send, and applies its change.
async fn wait_for_send(state: &RefCell<State>, ep: Ep) {
    loop {
        let next = state.borrow_mut().next_report(ep, now());
        match next {
            Next::Now => return,
            Next::At(at) => {
                select(device::kick(ep).wait(), Timer::at(Instant::from_micros(at))).await;
            }
            Next::Idle => device::kick(ep).wait().await,
        }
    }
}

/// Waits until the host takes `ep`'s pending report (or it is revoked),
/// polling the buffer's AVAILABLE bit and yielding between polls.
async fn complete(state: &RefCell<State>, ep: Ep, index: usize) {
    while !state.borrow_mut().poll_completion(ep, index) {
        yield_now().await;
    }
}

fn answer(answers: &Answers, a: &Answer<'_>) {
    let mut line = Line::new();
    if write!(line, "{a}").is_ok() {
        let _ = answers.try_send(line);
    }
}

/// Reads command lines; also watches the link's control lines: a line
/// coding of 1200 baud reboots into BOOTSEL (whatever the parser does), and
/// DTR dropped disarms.
async fn receive<'d, D: embassy_usb::driver::Driver<'d>>(
    rx: &mut Receiver<'d, D>,
    control: &ControlChanged<'d>,
    state: &RefCell<State>,
    answers: &Answers,
    watchdog: &RefCell<Watchdog>,
) {
    let mut packet = [0_u8; 64];
    let mut line: heapless::Vec<u8, { protocol::MAX_LINE }> = heapless::Vec::new();
    let mut overlong = false;
    loop {
        match select(rx.read_packet(&mut packet), control.control_changed()).await {
            Either::First(Ok(n)) => {
                for &b in packet.get(..n).unwrap_or(&[]) {
                    if b == b'\n' || b == b'\r' {
                        if !line.is_empty() && !overlong {
                            run(&line, state, answers, watchdog).await;
                        }
                        line.clear();
                        overlong = false;
                    } else if line.push(b).is_err() {
                        overlong = true;
                    }
                }
            }
            Either::First(Err(_)) => rx.wait_connection().await,
            Either::Second(()) => {
                let baud_off = state.borrow().fault(Fault::BaudOff).is_some();
                if rx.line_coding().data_rate() == 1200 && !baud_off {
                    enter_bootsel(&mut watchdog.borrow_mut());
                }
                if !control.dtr() {
                    state.borrow_mut().disarm(Reason::Closed);
                }
            }
        }
    }
}

/// Writes one line in packets, ending a full last packet with a short one
/// so the host completes the transfer. Gives up on a host that stops
/// reading.
async fn write_line<'d, D: embassy_usb::driver::Driver<'d>>(tx: &mut Sender<'d, D>, line: &[u8]) {
    if !tx.dtr() {
        return;
    }
    let mut chunks = line.chunks(64).peekable();
    let mut last_full = false;
    while let Some(chunk) = chunks.next() {
        let sent = select(tx.write_packet(chunk), Timer::after_millis(100)).await;
        if !matches!(sent, Either::First(Ok(()))) {
            return;
        }
        last_full = chunks.peek().is_none() && chunk.len() == 64;
    }
    if last_full {
        let _ = select(tx.write_packet(&[]), Timer::after_millis(100)).await;
    }
}

/// Sends answers and trace records in the order made.
async fn send<'d, D: embassy_usb::driver::Driver<'d>>(
    tx: &mut Sender<'d, D>,
    state: &RefCell<State>,
    answers: &Answers,
) {
    loop {
        if let Either::First(mut line) = select(answers.receive(), device::KICK_TRACE.wait()).await
        {
            let _ = line.push('\n');
            write_line(tx, line.as_bytes()).await;
        }
        loop {
            let record = state.borrow_mut().ring.pop();
            let Some(r) = record else { break };
            let mut line = Line::new();
            if writeln!(line, "{r}").is_ok() {
                write_line(tx, line.as_bytes()).await;
            }
        }
    }
}

/// Runs one command line.
async fn run(line: &[u8], state: &RefCell<State>, answers: &Answers, watchdog: &RefCell<Watchdog>) {
    if state.borrow().fault(Fault::ParserOff).is_some() {
        return;
    }
    let Some(cmd) = core::str::from_utf8(line).ok().and_then(Command::parse) else {
        answer(answers, &Answer::Err(Error::Syntax));
        return;
    };
    let reply = match cmd {
        Command::Hello => {
            let mut s = state.borrow_mut();
            s.hello_answered = true;
            let hello = Hello {
                version: protocol::VERSION,
                build: env!("TEST_DEVICE_BUILD"),
                mode: s.mode,
                lease: s.lease.held(),
                armed: s.lease.armed(),
                reason: s.lease.reason(),
                boot: s.boot,
                count: s.count,
                identified: s.identified(),
            };
            answer(answers, &Answer::Hello(hello));
            return;
        }
        Command::Mode(mode) => {
            watchdog.borrow_mut().set_scratch(0, mode.scratch());
            answer(answers, &Answer::Ok);
            Timer::after_millis(50).await;
            watchdog.borrow_mut().trigger_reset();
            return;
        }
        Command::Lease { ms } => {
            let mut s = state.borrow_mut();
            let granted = s.lease.grant(now(), ms);
            device::kick(Ep::Pad).signal(());
            match granted {
                Ok((id, deadline)) => Answer::Leased {
                    id,
                    deadline: s.out(deadline),
                },
                Err(r) => Answer::Err(Error::Lease(r)),
            }
        }
        Command::Renew { id } => {
            let mut s = state.borrow_mut();
            match s.lease.renew(id, now()) {
                Ok(deadline) => Answer::Leased {
                    id,
                    deadline: s.out(deadline),
                },
                Err(r) => Answer::Err(Error::Lease(r)),
            }
        }
        Command::Set(change) => {
            let mut s = state.borrow_mut();
            if s.lease.held().is_none() {
                Answer::Err(Error::Lease(test_device::lease::Refusal::NoLease))
            } else if s.schedule.armed() {
                Answer::Err(Error::Playing)
            } else {
                let ep = change.control.ep();
                if s.ep(ep).changes.push_back(change).is_ok() {
                    device::kick(ep).signal(());
                    Answer::Ok
                } else {
                    Answer::Err(Error::Pattern)
                }
            }
        }
        Command::Load => {
            let mut s = state.borrow_mut();
            if s.schedule.armed() {
                Answer::Err(Error::Playing)
            } else {
                s.schedule.clear();
                Answer::Ok
            }
        }
        Command::Item(item) => match state.borrow_mut().schedule.push(item) {
            Ok(()) => Answer::Ok,
            Err(Refused::Armed) => Answer::Err(Error::Playing),
            Err(Refused::Full | Refused::Order) => Answer::Err(Error::Pattern),
        },
        Command::Arm { id, at } => {
            let mut s = state.borrow_mut();
            if s.identified() {
                match s.lease.arm(id) {
                    Ok(()) => {
                        let start = s.inward(at);
                        s.schedule.arm(start);
                        device::kick(Ep::Keyboard).signal(());
                        device::kick(Ep::Pad).signal(());
                        Answer::Ok
                    }
                    Err(r) => Answer::Err(Error::Lease(r)),
                }
            } else {
                Answer::Err(Error::Unidentified)
            }
        }
        Command::Ping(n) => {
            let s = state.borrow();
            let shift = s.fault(Fault::PongOffset).unwrap_or(0);
            let at = s.out(now()).wrapping_add_signed(shift);
            Answer::Pong { n, at }
        }
        Command::Trace(on) => {
            let mut s = state.borrow_mut();
            if on && !s.identified() {
                Answer::Err(Error::Unidentified)
            } else {
                s.tracing = on;
                Answer::Ok
            }
        }
        Command::Flush => {
            flush(state).await;
            return;
        }
        Command::Release => {
            state.borrow_mut().disarm(Reason::Released);
            Answer::Ok
        }
        Command::Bootsel => {
            answer(answers, &Answer::Ok);
            Timer::after_millis(20).await;
            enter_bootsel(&mut watchdog.borrow_mut());
        }
        Command::Detach { ms } => {
            state.borrow_mut().disarm(Reason::Detached);
            answer(answers, &Answer::Ok);
            Timer::after_millis(20).await;
            embassy_rp::pac::USB
                .sie_ctrl()
                .modify(|w| w.set_pullup_en(false));
            Timer::after_millis(u64::from(ms.min(MAX_DETACH_MS))).await;
            embassy_rp::pac::USB
                .sie_ctrl()
                .modify(|w| w.set_pullup_en(true));
            return;
        }
        Command::Fault { fault, arg } => fault_command(state, watchdog, fault, arg),
    };
    answer(answers, &reply);
}

/// Waits until every endpoint's pending report completes, bounded by two
/// intervals plus 10 ms; then records each still pending as `P` and ends
/// with `F`, after every buffered record.
async fn flush(state: &RefCell<State>) {
    let interval = u64::from(state.borrow().mode.rate.interval_us());
    let deadline = now() + 2 * interval + 10_000;
    loop {
        let clear = {
            let mut s = state.borrow_mut();
            s.poll_completion(Ep::Keyboard, KEYBOARD_EP) && s.poll_completion(Ep::Pad, PAD_EP)
        };
        if clear || now() >= deadline {
            break;
        }
        yield_now().await;
    }
    let mut s = state.borrow_mut();
    for ep in [Ep::Keyboard, Ep::Pad] {
        if let Some(seq) = s.ep(ep).assoc.pending() {
            s.ring.push(Body::Pending { ep, seq });
        }
    }
    s.ring.push(Body::Flushed);
    device::KICK_TRACE.signal(());
}

#[cfg(feature = "debug-faults")]
fn fault_command(
    state: &RefCell<State>,
    watchdog: &RefCell<Watchdog>,
    fault: Fault,
    arg: i64,
) -> Answer<'static> {
    let mut s = state.borrow_mut();
    let t = now();
    match fault {
        Fault::Hang => device::hang(),
        Fault::BootHang => {
            let boots = u16::try_from(arg).unwrap_or(0);
            device::set_boot_hang(&mut watchdog.borrow_mut(), boots);
            device::hang();
        }
        Fault::Suspend => {
            s.faults.set(fault, arg, t);
            s.disarm(Reason::Suspended);
        }
        Fault::ClockOffset => {
            // The reported µs start 10 s before 2^32.
            s.offset = ((1_u64 << 32) - 10_000_000).saturating_sub(t);
            s.faults.set(fault, arg, t);
        }
        _ => s.faults.set(fault, arg, t),
    }
    Answer::Ok
}

#[cfg(not(feature = "debug-faults"))]
fn fault_command(
    _state: &RefCell<State>,
    _watchdog: &RefCell<Watchdog>,
    _fault: Fault,
    _arg: i64,
) -> Answer<'static> {
    Answer::Err(Error::NoFaults)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // Nothing feeds the watchdog any more: it resets the chip, and three
    // such resets in a row reach BOOTSEL (the boot counter).
    loop {
        cortex_m::asm::wfi();
    }
}
