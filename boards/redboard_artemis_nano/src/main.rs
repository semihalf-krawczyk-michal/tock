//! Board file for SparkFun Redboard Artemis Nano
//!
//! - <https://www.sparkfun.com/products/15443>

#![no_std]
// Disable this attribute when documenting, as a workaround for
// https://github.com/rust-lang/rust/issues/62184.
#![cfg_attr(not(doc), no_main)]
#![deny(missing_docs)]

use apollo3::chip::Apollo3DefaultPeripherals;
use capsules::virtual_alarm::VirtualMuxAlarm;
use kernel::capabilities;
use kernel::common::dynamic_deferred_call::DynamicDeferredCall;
use kernel::common::dynamic_deferred_call::DynamicDeferredCallClientState;
use kernel::component::Component;
use kernel::hil::i2c::I2CMaster;
use kernel::hil::led::LedHigh;
use kernel::hil::time::Counter;
use kernel::Platform;
use kernel::{create_capability, debug, static_init};

pub mod ble;
/// Support routines for debugging I/O.
pub mod io;

#[allow(dead_code)]
mod multi_alarm_test;

// Number of concurrent processes this platform supports.
const NUM_PROCS: usize = 4;

// Actual memory for holding the active process structures.
static mut PROCESSES: [Option<&'static dyn kernel::procs::ProcessType>; NUM_PROCS] = [None; 4];

// Static reference to chip for panic dumps.
static mut CHIP: Option<&'static apollo3::chip::Apollo3<Apollo3DefaultPeripherals>> = None;

// How should the kernel respond when a process faults.
const FAULT_RESPONSE: kernel::procs::FaultResponse = kernel::procs::FaultResponse::Panic;

/// Dummy buffer that causes the linker to reserve enough space for the stack.
#[no_mangle]
#[link_section = ".stack_buffer"]
pub static mut STACK_MEMORY: [u8; 0x1000] = [0; 0x1000];

/// A structure representing this platform that holds references to all
/// capsules for this platform.
struct RedboardArtemisNano {
    alarm: &'static capsules::alarm::AlarmDriver<
        'static,
        VirtualMuxAlarm<'static, apollo3::stimer::STimer<'static>>,
    >,
    led: &'static capsules::led::LedDriver<
        'static,
        LedHigh<'static, apollo3::gpio::GpioPin<'static>>,
    >,
    gpio: &'static capsules::gpio::GPIO<'static, apollo3::gpio::GpioPin<'static>>,
    console: &'static capsules::console::Console<'static>,
    i2c_master: &'static capsules::i2c_master::I2CMasterDriver<apollo3::iom::Iom<'static>>,
    ble_radio: &'static capsules::ble_advertising_driver::BLE<
        'static,
        apollo3::ble::Ble<'static>,
        VirtualMuxAlarm<'static, apollo3::stimer::STimer<'static>>,
    >,
}

/// Mapping of integer syscalls to objects that implement syscalls.
impl Platform for RedboardArtemisNano {
    fn with_driver<F, R>(&self, driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<Result<&dyn kernel::Driver, &dyn kernel::LegacyDriver>>) -> R,
    {
        match driver_num {
            capsules::alarm::DRIVER_NUM => f(Some(Ok(self.alarm))),
            capsules::led::DRIVER_NUM => f(Some(Ok(self.led))),
            capsules::gpio::DRIVER_NUM => f(Some(Ok(self.gpio))),
            capsules::console::DRIVER_NUM => f(Some(Ok(self.console))),
            capsules::i2c_master::DRIVER_NUM => f(Some(Ok(self.i2c_master))),
            capsules::ble_advertising_driver::DRIVER_NUM => f(Some(Ok(self.ble_radio))),
            _ => f(None),
        }
    }
}

/// Reset Handler.
///
/// This symbol is loaded into vector table by the Apollo3 chip crate.
/// When the chip first powers on or later does a hard reset, after the core
/// initializes all the hardware, the address of this function is loaded and
/// execution begins here.
#[no_mangle]
pub unsafe fn reset_handler() {
    apollo3::init();

    let peripherals = static_init!(Apollo3DefaultPeripherals, Apollo3DefaultPeripherals::new());

    // No need to statically allocate mcu/pwr/clk_ctrl because they are only used in main!
    let mcu_ctrl = apollo3::mcuctrl::McuCtrl::new();
    let pwr_ctrl = apollo3::pwrctrl::PwrCtrl::new();
    let clkgen = apollo3::clkgen::ClkGen::new();

    clkgen.set_clock_frequency(apollo3::clkgen::ClockFrequency::Freq48MHz);

    // initialize capabilities
    let process_mgmt_cap = create_capability!(capabilities::ProcessManagementCapability);
    let main_loop_cap = create_capability!(capabilities::MainLoopCapability);
    let memory_allocation_cap = create_capability!(capabilities::MemoryAllocationCapability);

    let dynamic_deferred_call_clients =
        static_init!([DynamicDeferredCallClientState; 1], Default::default());
    let dynamic_deferred_caller = static_init!(
        DynamicDeferredCall,
        DynamicDeferredCall::new(dynamic_deferred_call_clients)
    );
    DynamicDeferredCall::set_global_instance(dynamic_deferred_caller);

    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(&PROCESSES));

    // Power up components
    pwr_ctrl.enable_uart0();
    pwr_ctrl.enable_iom2();

    // Enable PinCfg
    &peripherals
        .gpio_port
        .enable_uart(&&peripherals.gpio_port[48], &&peripherals.gpio_port[49]);
    // Enable SDA and SCL for I2C2 (exposed via Qwiic)
    &peripherals
        .gpio_port
        .enable_i2c(&&peripherals.gpio_port[25], &&peripherals.gpio_port[27]);

    // Configure kernel debug gpios as early as possible
    kernel::debug::assign_gpios(
        Some(&peripherals.gpio_port[19]), // Blue LED
        None,
        None,
    );

    // Create a shared UART channel for the console and for kernel debug.
    let uart_mux = components::console::UartMuxComponent::new(
        &peripherals.uart0,
        115200,
        dynamic_deferred_caller,
    )
    .finalize(());

    // Setup the console.
    let console = components::console::ConsoleComponent::new(board_kernel, uart_mux).finalize(());
    // Create the debugger object that handles calls to `debug!()`.
    components::debug_writer::DebugWriterComponent::new(uart_mux).finalize(());

    // LEDs
    let led = components::led::LedsComponent::new(components::led_component_helper!(
        LedHigh<'static, apollo3::gpio::GpioPin>,
        LedHigh::new(&peripherals.gpio_port[19]),
    ))
    .finalize(components::led_component_buf!(
        LedHigh<'static, apollo3::gpio::GpioPin>
    ));

    // GPIOs
    // These are also ADC channels, but let's expose them as GPIOs
    let gpio = components::gpio::GpioComponent::new(
        board_kernel,
        components::gpio_component_helper!(
            apollo3::gpio::GpioPin,
            0 => &&peripherals.gpio_port[13],  // A0
            1 => &&peripherals.gpio_port[33],  // A1
            2 => &&peripherals.gpio_port[11],  // A2
            3 => &&peripherals.gpio_port[29],  // A3
            5 => &&peripherals.gpio_port[31]  // A5
        ),
    )
    .finalize(components::gpio_component_buf!(apollo3::gpio::GpioPin));

    // Create a shared virtualisation mux layer on top of a single hardware
    // alarm.
    peripherals.stimer.start();
    let mux_alarm = components::alarm::AlarmMuxComponent::new(&peripherals.stimer).finalize(
        components::alarm_mux_component_helper!(apollo3::stimer::STimer),
    );
    let alarm = components::alarm::AlarmDriverComponent::new(board_kernel, mux_alarm)
        .finalize(components::alarm_component_helper!(apollo3::stimer::STimer));

    // Init the I2C device attached via Qwiic
    let i2c_master = static_init!(
        capsules::i2c_master::I2CMasterDriver<apollo3::iom::Iom<'static>>,
        capsules::i2c_master::I2CMasterDriver::new(
            &peripherals.iom2,
            &mut capsules::i2c_master::BUF,
            board_kernel.create_grant(&memory_allocation_cap)
        )
    );

    &peripherals.iom2.set_master_client(i2c_master);
    &peripherals.iom2.enable();

    // Setup BLE
    mcu_ctrl.enable_ble();
    clkgen.enable_ble();
    pwr_ctrl.enable_ble();
    &peripherals.ble.setup_clocks();
    mcu_ctrl.reset_ble();
    &peripherals.ble.power_up();
    &peripherals.ble.ble_initialise();

    let ble_radio = ble::BLEComponent::new(board_kernel, &peripherals.ble, mux_alarm).finalize(());

    mcu_ctrl.print_chip_revision();

    debug!("Initialization complete. Entering main loop");

    /// These symbols are defined in the linker script.
    extern "C" {
        /// Beginning of the ROM region containing app images.
        static _sapps: u8;
        /// End of the ROM region containing app images.
        static _eapps: u8;
        /// Beginning of the RAM region for app memory.
        static mut _sappmem: u8;
        /// End of the RAM region for app memory.
        static _eappmem: u8;
    }

    let artemis_nano = static_init!(
        RedboardArtemisNano,
        RedboardArtemisNano {
            alarm,
            console,
            gpio,
            led,
            i2c_master,
            ble_radio,
        }
    );

    let chip = static_init!(
        apollo3::chip::Apollo3<Apollo3DefaultPeripherals>,
        apollo3::chip::Apollo3::new(peripherals)
    );
    CHIP = Some(chip);

    kernel::procs::load_processes(
        board_kernel,
        chip,
        core::slice::from_raw_parts(
            &_sapps as *const u8,
            &_eapps as *const u8 as usize - &_sapps as *const u8 as usize,
        ),
        core::slice::from_raw_parts_mut(
            &mut _sappmem as *mut u8,
            &_eappmem as *const u8 as usize - &_sappmem as *const u8 as usize,
        ),
        &mut PROCESSES,
        FAULT_RESPONSE,
        &process_mgmt_cap,
    )
    .unwrap_or_else(|err| {
        debug!("Error loading processes!");
        debug!("{:?}", err);
    });

    let scheduler = components::sched::round_robin::RoundRobinComponent::new(&PROCESSES)
        .finalize(components::rr_component_helper!(NUM_PROCS));

    board_kernel.kernel_loop(
        artemis_nano,
        chip,
        None::<&kernel::ipc::IPC<NUM_PROCS>>,
        scheduler,
        &main_loop_cap,
    );
}
