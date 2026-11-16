//! The test input device's firmware (docs/test-device.md): one composite
//! USB device on the Pico 2 with a keyboard (interface 0), a game pad
//! (interface 1) and a control link (CDC ACM, interfaces 2-3). Every input
//! is synthesized from commands and patterns received over the link. The
//! logic is the `test-device` crate's ([`test_device::device`]); this
//! crate runs it on the USB controller, the timer and the watchdog.
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
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State as CdcState};
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidState,
};
use embassy_usb::control::{OutResponse, Recipient, Request, RequestType};
use embassy_usb::types::InterfaceNumber;
use embassy_usb::{Builder, Config, Handler};
use static_cell::ConstStaticCell;
use test_device::boot::{self, Action, Cause};
use test_device::device::{Hardware as _, Kind, Link, Next, Outcome, State};
use test_device::faults::Fault;
use test_device::lease::Reason;
use test_device::mode::Mode;
#[cfg(not(feature = "debug-faults"))]
use test_device::protocol::Error;
use test_device::protocol::{self, Answer};
use test_device::report::{KEYBOARD_DESCRIPTOR, KEYBOARD_REPORT, PAD_DESCRIPTOR, PAD_REPORT};
use test_device::trace::Ep;

use crate::device::Pico;

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
/// The interfaces: the keyboard's, the pad's, and the link's CDC
/// communication interface (the module doc).
const KEYBOARD_INTERFACE: u8 = 0;
const PAD_INTERFACE: u8 = 1;
const LINK_INTERFACE: u16 = 2;

type Line = heapless::String<{ protocol::MAX_LINE }>;
type Answers = Channel<NoopRawMutex, Line, 8>;

/// The µs since boot.
fn now() -> u64 {
    Pico.now()
}

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
    state.borrow_mut().set_boot(mode, cause, count);
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
    let (mut tx, mut rx) = cdc.split();
    let mut usb = builder.build();
    let answers = Answers::new();

    let keyboard_loop = async {
        loop {
            keyboard.ready().await;
            let kind = wait_for_send(state, Ep::Keyboard).await;
            let report = state.borrow_mut().keyboard_report(&Pico, kind);
            if keyboard.write(&report).await.is_ok() {
                state.borrow_mut().handed(&Pico, Ep::Keyboard);
            }
            complete(state, Ep::Keyboard).await;
        }
    };
    let pad_loop = async {
        loop {
            pad.ready().await;
            let kind = wait_for_send(state, Ep::Pad).await;
            let report = state.borrow_mut().pad_report(&Pico, kind);
            if pad.write(&report).await.is_ok() {
                state.borrow_mut().handed(&Pico, Ep::Pad);
            }
            complete(state, Ep::Pad).await;
            let skip = state.borrow().fault(&Pico, Fault::EveryOtherPoll).is_some();
            if skip {
                let interval = state.borrow().mode().rate.interval_us();
                Timer::after_micros(u64::from(interval)).await;
            }
        }
    };
    let link_in = receive(&mut rx, state, &answers, &watchdog);
    let link_out = send(&mut tx, state, &answers);
    let lease_timer = async {
        loop {
            Timer::after(LEASE_TICK).await;
            let mut s = state.borrow_mut();
            s.expire(&Pico);
            let t = now();
            if s.clear_counter_due(t) {
                watchdog.borrow_mut().set_scratch(1, boot::word(0));
            }
            if s.recovery_due(t) {
                // An abort went unacknowledged: leaving the bus by a reboot
                // ends every input the host holds from this device.
                watchdog.borrow_mut().trigger_reset();
            }
            if s.bootsel_due() {
                enter_bootsel(&mut watchdog.borrow_mut());
            }
        }
    };
    let feeder = async {
        loop {
            let slow = state.borrow().fault(&Pico, Fault::SlowWatchdog).is_some();
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

/// The bus events that disarm, the configuration requests that
/// reinitialise the endpoints, and the link's control requests. embassy-usb
/// calls each hook in the same synchronous step as the change it reports,
/// so the logic hears of a reinitialised endpoint before any completion
/// poll and of a link request whatever the link's reader waits on.
/// Registered before the classes, it sees each request first and leaves
/// it to the CDC class.
struct Bus {
    state: &'static RefCell<State>,
}

impl Handler for Bus {
    fn reset(&mut self) {
        self.state.borrow_mut().bus_reset(&Pico);
    }

    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if (req.request_type, req.recipient, req.index)
            == (RequestType::Class, Recipient::Interface, LINK_INTERFACE)
        {
            self.state
                .borrow_mut()
                .cdc_request(&Pico, req.request, req.value, data);
        }
        None
    }

    fn suspended(&mut self, suspended: bool) {
        if suspended {
            self.state.borrow_mut().disarm(&Pico, Reason::Suspended);
        }
    }

    fn configured(&mut self, configured: bool) {
        let mut s = self.state.borrow_mut();
        if configured {
            s.configured(&Pico);
        } else {
            s.unconfigured(&Pico);
        }
    }

    fn set_alternate_setting(&mut self, iface: InterfaceNumber, _alternate_setting: u8) {
        let ep = match iface.0 {
            KEYBOARD_INTERFACE => Ep::Keyboard,
            PAD_INTERFACE => Ep::Pad,
            _ => return,
        };
        self.state.borrow_mut().interface_set(&Pico, ep);
    }
}

/// Waits until `ep` has a report to send, and applies its change: what
/// the report carries.
async fn wait_for_send(state: &RefCell<State>, ep: Ep) -> Kind {
    loop {
        let next = state.borrow_mut().next_report(&Pico, ep);
        match next {
            Next::Now(kind) => return kind,
            Next::At(at) => {
                select(device::kick(ep).wait(), Timer::at(Instant::from_micros(at))).await;
            }
            Next::Idle => device::kick(ep).wait().await,
        }
    }
}

/// Waits until the host takes `ep`'s pending report (or it is revoked),
/// polling the buffer's AVAILABLE bit and yielding between polls.
async fn complete(state: &RefCell<State>, ep: Ep) {
    while !state.borrow_mut().poll_completion(&Pico, ep) {
        yield_now().await;
    }
}

fn answer(answers: &Answers, a: &Answer<'_>) {
    let mut line = Line::new();
    if write!(line, "{a}").is_ok() {
        let _ = answers.try_send(line);
    }
}

/// Reads command lines in packets through [`Link`], which keeps them
/// within one connection of the link; [`Bus`] takes the control requests
/// that end one. A failed read (a disconnect) ends one too.
async fn receive<'d, D: embassy_usb::driver::Driver<'d>>(
    rx: &mut Receiver<'d, D>,
    state: &RefCell<State>,
    answers: &Answers,
    watchdog: &RefCell<Watchdog>,
) {
    let mut packet = [0_u8; 64];
    let mut link = Link::new();
    loop {
        match rx.read_packet(&mut packet).await {
            Ok(n) => {
                let bytes = packet.get(..n).unwrap_or(&[]);
                let mut at = 0;
                loop {
                    // Not borrowed across the outcome's await.
                    let outcome = link.next(&mut state.borrow_mut(), &Pico, bytes, &mut at);
                    let Some(outcome) = outcome else { break };
                    run(outcome, state, answers, watchdog).await;
                }
            }
            Err(_) => {
                state.borrow_mut().link_lost();
                rx.wait_connection().await;
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
            let record = state.borrow_mut().next_record();
            let Some(r) = record else { break };
            let mut line = Line::new();
            if writeln!(line, "{r}").is_ok() {
                write_line(tx, line.as_bytes()).await;
            }
        }
    }
}

/// Finishes what the device logic hands back for one command.
async fn run(
    outcome: Outcome,
    state: &RefCell<State>,
    answers: &Answers,
    watchdog: &RefCell<Watchdog>,
) {
    match outcome {
        Outcome::Answer(a) => answer(answers, &a),
        Outcome::Hello => {
            let hello = state.borrow_mut().hello(env!("TEST_DEVICE_BUILD"));
            answer(answers, &Answer::Hello(hello));
        }
        Outcome::Mode(mode) => {
            watchdog.borrow_mut().set_scratch(0, mode.scratch());
            answer(answers, &Answer::Ok);
            Timer::after_millis(50).await;
            watchdog.borrow_mut().trigger_reset();
        }
        Outcome::Flush => flush(state).await,
        Outcome::Bootsel => {
            answer(answers, &Answer::Ok);
            Timer::after_millis(20).await;
            enter_bootsel(&mut watchdog.borrow_mut());
        }
        Outcome::Detach(ms) => {
            answer(answers, &Answer::Ok);
            Timer::after_millis(20).await;
            embassy_rp::pac::USB
                .sie_ctrl()
                .modify(|w| w.set_pullup_en(false));
            Timer::after_millis(u64::from(ms.min(MAX_DETACH_MS))).await;
            embassy_rp::pac::USB
                .sie_ctrl()
                .modify(|w| w.set_pullup_en(true));
        }
        Outcome::Fault { fault, arg } => {
            let a = fault_command(state, watchdog, fault, arg);
            answer(answers, &a);
        }
    }
}

/// Waits until every endpoint's pending report completes, bounded by two
/// intervals plus 10 ms; then records each still pending as `P` and ends
/// with `F`, after every buffered record.
async fn flush(state: &RefCell<State>) {
    let interval = u64::from(state.borrow().mode().rate.interval_us());
    let deadline = now() + 2 * interval + 10_000;
    loop {
        let clear = state.borrow_mut().flush_poll(&Pico);
        if clear || now() >= deadline {
            break;
        }
        yield_now().await;
    }
    state.borrow_mut().flush_end(&Pico);
}

#[cfg(feature = "debug-faults")]
fn fault_command(
    state: &RefCell<State>,
    watchdog: &RefCell<Watchdog>,
    fault: Fault,
    arg: i64,
) -> Answer<'static> {
    match fault {
        Fault::Hang => device::hang(),
        Fault::BootHang => {
            let boots = u16::try_from(arg).unwrap_or(0);
            device::set_boot_hang(&mut watchdog.borrow_mut(), boots);
            device::hang();
        }
        _ => state.borrow_mut().set_fault(&Pico, fault, arg),
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
