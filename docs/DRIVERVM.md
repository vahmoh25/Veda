# The driver VM

Veda drives the devices it depends on itself — disks, keyboards and mice,
the screen it starts on — and takes the rest from Linux: the GPU, audio,
Wi-Fi, Bluetooth, cameras, and the thousands of devices Linux has drivers
for. Linux runs in a virtual machine of Veda's own, *the driver VM*, which
gets those devices; its drivers serve Veda's services through the same
protocols Veda's own drivers speak. A driver that crashes, hangs or is
compromised takes down a virtual machine that Veda restarts, never Veda.

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Veda: services (audio, compositor, netd, wlan…)   native drivers (disks, │
 │       speak audiodev, displaydev, netdev, gpu…     input, firmware fb)   │
 │                     │                                                    │
 │             drivervm (the monitor: memory, processors, hypercalls,       │
 │             the bridge, the registry the guest sees, devices it gets)    │
 ├─────────────────────│──────────────────────────────────────────────────── │
 │ vkernel: Guest (EPT) · Vcpu (VMX, x2APIC) · IOMMU domains · interrupts   │
 ╞═════════════════════│════════════════════════════════════════════════════╡
 │ Linux (the guest)   │                                                    │
 │   programs: Veda's drivers for Linux (vrt over /dev/veda): ALSA →        │
 │   audiodev, KMS → displaydev, Mesa → gpu, netdev/nl80211 → netdev, …    │
 │   kernel: Linux's drivers (i915/xe, amdgpu, snd-hda/SOF, iwlwifi, mt76,  │
 │   btusb…) + the Veda platform (console, bridge, PCI, processors)         │
 └──────────────────────────────────────────────────────────────────────────┘
        devices given to the guest: their BARs in its memory, their DMA
        through the IOMMU into its memory only, their interrupts remapped
```

## Why

Veda's own drivers cover what a PC needs to start and be used: storage,
input, the firmware's framebuffer, a few sound and network chips, Intel's
display engine and render engines of a few generations. A PC has much
more, and every new generation of GPUs, Wi-Fi chips and audio DSPs needs
years of driver work with the vendors' documentation and firmware. Linux
has that work done, for nearly everything. The question is how to use it
without giving up what Veda is: a capability system where drivers are
untrusted, isolated processes.

Linux's drivers expect Linux: its memory management, interrupts, timers,
locking, PCI and firmware interfaces, DMA API, power management. Porting
them one by one (as Genode's DDE does, or HarmonyOS's driver containers)
means emulating a growing surface of Linux's kernel for every driver, and
following it as it changes. Running Linux itself, unmodified but for a few
platform files, keeps every driver as its authors tested it and makes
Linux's own releases the driver updates. A virtual machine is the cleanest
boundary there is around a piece of code that big: the processor's
virtualization and the IOMMU confine it to its memory and its devices.

## Decisions

Each one came from what the alternatives would cost.

1. **The boundary is Veda's own driver protocols.** Linux's drivers serve
   Veda through `audiodev`, `displaydev`, `netdev`, `gpu`… exactly as
   Veda's native drivers do, carried across the VM boundary by the
   *bridge*. Veda's services cannot tell a Linux-backed driver from a
   native one, and need no change. The alternatives were worse: exporting
   virtio devices from the guest needs a device-side virtio implementation
   per class and adds a translation layer (virtio-snd between ALSA and
   `audiodev`, virtio-gpu between Mesa and `gpu`), and loses what Veda's
   protocols carry that virtio's do not (audio clocks, flip timestamps,
   Wi-Fi state); using Linux's user-space APIs from Veda would make Veda's
   services Linux-specific.

2. **The bridge is Veda's system call interface, carried into the guest.**
   A program in the guest uses `vrt`, Veda's runtime, built with
   `--cfg veda_guest`: its system calls on Veda's objects (channels,
   events, VMOs, waits) become operations on `/dev/veda`, which the guest's
   kernel turns into hypercalls, which the monitor carries out on a handle
   table of the guest's own. So `vipc` and `vproto` — every protocol Veda
   has — work in the guest unchanged; a driver for Linux is written exactly
   like a Veda driver, against Linux's device interfaces instead of the
   hardware. The objects stay in Veda; the guest holds numbers that only
   the monitor gives meaning to, so it reaches exactly what it was given.

3. **The guest is fully paravirtualized.** There is no emulated hardware:
   no PIC, PIT, I/O APIC, RTC, serial port, firmware or ACPI. Processors
   have an x2APIC (with the TSC-deadline timer) and start through a
   hypercall; the console, power, the time of day, PCI configuration and
   interrupts of the devices are hypercalls. A small patch to Linux
   (`ports/linux`, modelled on Linux's Jailhouse and ACRN guests) makes it
   the *Veda platform*. In return the hypervisor emulates no instruction
   at all — no MMIO decoding, the largest source of hypervisor bugs — and
   Linux boots in about 0.1 s.

4. **The kernel keeps to mechanism.** `vkernel` gives a monitor process
   `Guest` objects (a guest-physical address space of VMOs, and later the
   IOMMU domain of its devices) and `Vcpu` objects (a virtual processor
   that a thread of the monitor runs), and handles in the kernel only what
   must be fast or belongs to the processor: the local APIC and its timer,
   `cpuid`, MSRs, `hlt`, control registers. Everything that is policy —
   the guest's memory map, how it boots, its hypercalls, the bridge, which
   devices it gets — is the monitor's, `drivervm`, an ordinary process.

5. **Devices are given whole, behind the IOMMU.** A PCI function given to
   the guest has its BARs mapped into the guest, its DMA translated by the
   IOMMU into the guest's memory and nothing else, and its interrupts
   remapped (so it can raise only its own: the IOMMU remaps all of Veda's
   interrupts, and refuses a device another's). Without an IOMMU no device
   is given away. Individual USB devices (a Bluetooth adapter, a camera) are
   tunnelled by Veda's own USB driver (USB/IP over the bridge), so the USB
   controller and the keyboard stay Veda's.

6. **Memory is shared, not copied.** A VMO a guest program maps is mapped
   into the guest's physical memory (a window beyond its RAM) and into the
   program; the other side maps the same pages. Data paths (audio rings,
   network rings, pictures, command buffers) cost no copy; what either
   side reads from shared memory, it treats as untrusted, as Veda's
   protocols already do.

7. **Time is shared.** The guest reads the host's TSC as it is (no offset,
   no scaling), and Veda's clocks follow the TSC: the guest computes Veda's
   monotonic time itself, so timestamps (when a period played, when a
   frame was flipped) mean the same on both sides with no call.

8. **The driver VM is restartable.** Its life is `drivervm`'s: when Linux
   crashes, the process ends, and the devices are reset and given to a new
   one. Veda's services already handle a driver going away (calls have
   timeouts, drivers attach again).

9. **Linux is a port.** The kernel is built from a pinned release with a
   configuration that turns on only what the platform has and the devices
   need (`ports/linux/veda.config`), and a patch for the Veda platform.
   The guest's programs are Rust, static, on musl; the initial RAM file
   system holds them and the firmware the devices need.

## The pieces

| Piece | Where | What it does |
|-------|-------|--------------|
| Hypervisor | `kernel/src/hv` | VMX and EPT; `Guest` and `Vcpu` objects; the virtual x2APIC; guest memory shootdowns |
| IOMMU | `kernel/src/iommu` | VT-d: every device's interrupts remapped; devices passed through, or in a guest's domain |
| Hypervisor logic | `lib/hv` (`vhv`) | the virtual local APIC, the guests' `cpuid`, the platform's ABI, Linux's boot protocol, the bridge's ABI, a given function's configuration space (host-tested) |
| IOMMU logic | `lib/iommu` (`viommu`) | the DMAR table; the units' registers, tables, entries and descriptors (host-tested) |
| Device manager | `services/devmgr` | which devices go to the driver VM (`drivervm.devices`), their `pcidev` channels |
| Monitor | `services/drivervm` | the machine, its hypercalls, its PCI functions, the bridge, the narrowed registry |
| Guest kernel | `ports/linux` | Linux with the Veda platform: `arch/x86/kernel/cpu/veda.c`, `arch/x86/pci/veda.c`, `drivers/tty/hvc/hvc_veda.c`, `drivers/virt/veda/bridge.c` |
| Guest runtime | `lib/rt/src/guest.rs` | `vrt`'s system calls through `/dev/veda` |
| Guest programs | `guest/` | `init`, Veda's drivers for Linux (`alsa`), `bridgetest`, `pcitest` |
| Build | `xtask/src/linux.rs`, `ports/linux/build.sh` | `cargo xtask linux`; the initramfs; the image's `linux/` |

### The platform

A guest finds the platform with `cpuid`: leaf `0x40000000` says
`VedaVedaVeda`; `0x40000001` gives the number of processors (local APIC ids
0 to n−1); `0x40000010` the TSC's and the APIC timer's frequencies. A
hypercall is `vmcall` with the call in `rax` and arguments in `rbx`, `rcx`,
`rdx`, `rsi`, `rdi` (`vhv::platform`):

| Call | Does |
|------|------|
| `CONSOLE_WRITE` | up to 32 bytes, in registers (so it works however early) |
| `START_CPU` | starts a processor in 64-bit mode at an address, on page tables that map the first 4 GiB |
| `POWER` | off, restart, crashed |
| `WALLCLOCK` | the time of day |
| `BRIDGE` | an operation of the bridge |
| `PCI_CONFIG_READ`, `PCI_CONFIG_WRITE` | the configuration space of a PCI function the guest has |
| `PCI_MSI` | routes an MSI of a function to a processor and vector; gives the message the function sends |

The monitor boots Linux through the x86 boot protocol's 64-bit entry
(`vhv::linux`): the kernel at the address it prefers, the initramfs at the
top of memory, a memory map that keeps the first 64 KiB (the GDT and page
tables the processors start on) and the legacy hole out.

### The hypervisor

`vcpu_run` enters the guest on the calling thread with the BKL released, as
user code runs. Host interrupts make the guest exit and are taken at once
(the guest never delays them); the scheduler preempts a virtual processor
as it preempts a thread; `hlt` blocks the thread until an interrupt or the
guest's timer. The guest's FPU and the MSRs it shares with the host (the
system call MSRs, `KERNEL_GS_BASE`, `TSC_AUX`) are swapped only when the
virtual processor leaves the kernel's loop, not at every exit. The VMCS is
cleared whenever it leaves, so a virtual processor may run on any CPU next;
there it forgets what an earlier run left in that CPU's TLB (`invvpid`,
`invept`). Unmapping guest memory shoots the translations down on every
CPU before the pages can be reused.

### Devices

A PCI function goes to the driver VM when the boot options name it
(`drivervm.devices=VID:DID,...`, for now): `devmgr` starts no driver of
Veda's for it and hands the driver VM its `pcidev` channel, as it hands a
driver its device. Through it the monitor gets what giving the function
away takes, and nothing more: its configuration space, its BARs, its MSIs,
and a resource that names it (`resource_kind::PCI`, by requester id),
which only the driver VM's functions have.

**DMA.** The kernel drives the IOMMU (`kernel/src/iommu`, Intel VT-d). At
boot every device is in the host's domain, which passes requests through:
Veda's own drivers reach memory as before. `guest_attach_device` puts the
function in its guest's domain, whose second-level page tables mirror the
guest's memory — every `guest_map` and `guest_unmap` updates the EPT and
the domain together, so a device reaches exactly what the guest's
processors reach, at the same addresses, and unmapping makes devices
forget the pages (an IOTLB invalidation) before they can be reused. The
function is attached before the guest runs, so it never does DMA the guest
programmed with host addresses. When the guest ends, its functions'
context entries go: they reach nothing until another guest takes them.

**Interrupts.** The IOMMU remaps every interrupt in Veda, not only the
guest's: the units share one remapping table, whose entry V raises vector
V of the boot processor, and the compatibility format (an interrupt that
names its processor and vector itself) is blocked. An MSI's entry names
its function (source validation), so a function raises only its own
interrupts, whatever the guest writes into its MSI or MSI-X registers.
I/O APIC entries are remappable too, validated as the I/O APIC's. An MSI
of the guest's function is an `Interrupt` object bound to a virtual
processor (`vcpu_bind_interrupt`): the kernel raises the guest's vector at
its local APIC straight from the interrupt, without the monitor.

**The guest's PCI.** Linux's own x86 PCI code scans bus 0 and claims the
BARs where they are; `arch/x86/pci/veda.c` gives it the configuration space
through hypercalls (`raw_pci_ops`), and an MSI parent domain above x86's
vector domain, modelled on Hyper-V's root partition: when Linux's vector
domain has picked a processor and a vector, composing the message routes
the function's MSI there (`PCI_MSI`) and returns the message, which Linux
writes into the function, MSI-X table included (it is in a BAR the guest
reaches directly). A move to another processor routes it again; the
message stays. The monitor's view of the configuration space
(`vhv::pci`) is the function's own but for what the platform owns: the
memory BARs hold the guest-physical addresses it mapped them at (3 GiB to
4 GiB, or above the bridge's window), and answer sizing; there is no
expansion ROM, I/O BAR or INTx. A function on the host's bus 0 keeps its
device number when it can, so drivers that look for a sibling where it is
on a PC find it.

**Errors.** The errors a function signals become the host's system
errors, which a PC may turn into NMIs, and an NMI stops Veda: a given
function reports none. The monitor turns its SERR# enable and its PCI
Express error reporting off before the guest runs, and they stay off,
whatever the guest writes.

What a function needs for this, and the limits for now: MSIs (or MSI-X),
memory BARs of whole pages, and its first 256 bytes of configuration space
(`devmgr` reaches no more yet); no memory the firmware keeps for it (an
RMRR: such a device stays the host's); the guest's memory is at most
3 GiB.

### Veda's drivers for Linux

A driver for Linux is a program of the guest's, written as a Veda driver
is, against Linux's device interface instead of the hardware. The guest's
`init` starts the ones its devices need (by PCI class), and again if they
end.

**Sound** (`guest/alsa`). Linux's first playback device, through ALSA's
kernel interface (the PCM and control ioctls; no library), is attached to
Veda's audio service as its output device, through `audiodev`, as Veda's
own drivers attach theirs. The service's mixed output comes through the
shared ring, a VMO of Veda's mapped into the program; the driver writes it
to the card a period (10 ms) at a time, a few periods ahead, and pads with
silence when it runs short while streams play (an underrun, which deepens
the queue). How much has played comes from the card's own count (ALSA's
delay), stamped with Veda's clock, which the guest reads itself: the audio
service's positions and latencies are as with a native driver. The card's
period interrupts pace the driver while it plays; after a while without
audio the card stops. The card's mixer is set once to its outputs at 0 dB
(from their dB scales), unmuted: the volume is the audio service's.

### The bridge

| Guest | Kernel of the guest | Monitor |
|-------|---------------------|---------|
| `vrt::object::Channel::write(bytes, handles)` | `ioctl(VEDA_IOC_CHANNEL_WRITE)`: copies the buffers in, checks the file holds the handles | moves the guest's handles out of its table, writes the message |
| `wait_many(items, deadline)` | `VEDA_IOC_WAIT`: finished at once, or sleeps until the ring says | polls; or keeps the wait for its waiter thread, which writes the result and raises the callback vector |
| `Mapping::new(vmo)` | `VEDA_IOC_VMO_MAP`, then `mmap` | maps the VMO into the window of guest-physical memory |

A guest program's handles are its own: the guest's kernel lets a file use
only handles it made or received, and closes what is left when the file
closes. The registry the guest gets lets it connect only to services that
drivers attach to, and register only services it provides.

## What others learned

Running a driver's own operating system in a virtual machine, for the
driver, is twenty years old: L4Ka's DD/OS ran unmodified Linux drivers,
each in its Linux, on a microkernel acting as hypervisor, within a few
percent of native network throughput
([LeVasseur et al., OSDI 2004](https://www.usenix.org/conference/osdi-04/unmodified-device-driver-reuse-and-improved-system-dependability-virtual-machines)).
Xen's driver domains, Qubes' `sys-net`, `sys-usb` and `sys-audio`, Ghaf's
system VMs, Spectrum's network VMs and seL4's LionsOS do it today;
Genode compiles Linux's drivers into components of its own instead (DDE
Linux), and HongMeng into a container on its kernel, rejecting virtual
machines for managing memory and scheduling twice. Veda differs from all
of them in being the host operating system and the hypervisor at once,
and in its drivers' protocols being the boundary. What they found, and
where Veda stands:

1. **Cut at the coarsest interface, and keep it asynchronous.** A
   generic layer inside Linux gives the lowest common denominator
   (LeVasseur); forwarding a GPU's API reaches 12–86% of native, while
   forwarding its kernel interface (ChromeOS's DRM native contexts)
   reaches 99% on Qualcomm's GPUs. Veda cuts at its own driver protocols, which are
   asynchronous and share memory (decision 1). For GPUs the plan is the
   same cut: Mesa's own drivers in Veda over a transport of Veda's (Mesa
   keeps it apart, in `vdrm`), the kernel's interface in the guest.
2. **What the guest writes is hostile.** Xen's backends were taken over
   through values they read twice from shared rings
   ([XSA-155](https://xenbits.xen.org/xsa/advisory-155.html)), and its
   frontends attacked through races in revoking grants
   ([XSA-396](https://xenbits.xen.org/xsa/advisory-396.html)). The
   monitor copies a request out of guest memory before it checks it,
   keeps the ring's indices itself (the guest's only clamp them), and
   bounds what a guest can make it keep: handles, and waits, pending or
   finished (a guest that collects nothing cannot grow the monitor,
   whose memory is Veda's). Veda's shared rings already trust no peer's
   position.
3. **No ambient access to the host's memory.** Qubes replaced a display
   protocol that needed the host to map any of a guest's pages with
   grants; Linux got virtio DMA through grants. The guest reaches the VMOs
   it was given, in its window, and nothing else of Veda's.
4. **Map bulk data, keep the mappings, copy small things.** Xen moved
   between flipping pages, copying and mapping them as TLB shootdowns
   dominated; mappings it kept needed limits and reclaiming. A guest
   program's rings and buffers stay mapped while it uses them; messages
   are copied; one unmap is one shootdown.
5. **Signal only when the peer waits.** Xen's rings, Hyper-V's VMBus and
   seL4's queues (whose protocol was model-checked) all hold signals
   back. Veda's protocols signal events, and the bridge one interrupt for
   all the finished waits; how often an event is signalled is still to
   be measured and tuned.
6. **DMA remapping, interrupt remapping, and errors.** Without interrupt
   remapping a driver domain forges MSIs
   ([CVE-2011-1898](https://nvd.nist.gov/vuln/detail/CVE-2011-1898)); with
   it, a device can still raise the host's NMIs through system errors
   ([XSA-59](https://xenbits.xen.org/xsa/advisory-59.txt)). No device is
   given away without both remappings; every interrupt in Veda is
   remapped and checked against its source; a given function reports no
   errors.
7. **The firmware's ties to devices.** Memory reserved for a device
   (RMRRs), Intel's integrated graphics (its OpRegion, stolen memory and
   expected address 00:02.0), and the ACPI table that describes a
   laptop's microphones (Ghaf passes NHLT to its audio VM) all leak into
   the driver VM. Devices with RMRRs stay the host's for now; the
   firmware's tables for the guest (which has no ACPI) come with the
   devices that need them.
8. **Reset is the weak point; restarting the whole VM is the fallback.**
   A function-level reset takes 100 ms and many devices have none (Qubes'
   users weaken isolation to get past it); AMD's GPUs need resets of
   their own. Restarting Xen's network backend cost 140–260 ms without
   the device (Xoar). A guest's devices reach nothing once it ends;
   resetting them and starting a new guest is a later phase, and Veda's
   services already cope with a driver going away.
9. **Suspend, device by device.** Qubes unloads its PCI drivers before
   suspending; Ghaf picks a policy per VM. Later.
10. **Fixed memory, fast boot.** Passthrough rules out ballooning
    (Qubes); Firecracker reaches its guest's init in 125 ms. The driver
    VM's memory is one committed VMO, and Linux reaches its init in about
    0.1 s.
11. **Narrow, typed control.** Genode's Wi-Fi driver takes a declarative
    configuration and reports access points; Ghaf filters the D-Bus it
    forwards from its network VM. Veda's own protocols (`wlanphy`,
    `netdev`, `audiodev`) are that boundary already.
12. **Own the presentation clock.** No system found exports a GPU VM's
    display to another system's compositor with its real vertical blank:
    virtio-gpu's emulated one has been wrong, Xen's display protocol is
    fixed at 60 Hz. `displaydev` carries real flip events and timestamps,
    which Linux's KMS has (the display phase).
13. **Few driver VMs to start with.** IOMMU groups put devices together,
    and combining several driver VMs is still hard elsewhere (seL4's
    libvmm). One driver VM for now; nothing in the design assumes one.

## Status

| Phase | State |
|-------|-------|
| Linux runs in Veda: hypervisor, monitor, platform, console, SMP, power | done (`tests/ui/drivervm.vts`) |
| The bridge: Veda's IPC in the guest | done (`tests/ui/drivervm-bridge.vts`) |
| Devices: IOMMU (DMA and interrupt remapping), PCI given to the guest | done (`tests/ui/iommu.vts`, `tests/ui/drivervm-pci.vts`) |
| Audio: ALSA → `audiodev` (playback) | done (`tests/ui/drivervm-audio.vts`) |
| Network and Wi-Fi: netdev, nl80211 → `netdev`, `wlanphy` | next |
| Display and GPU: KMS → `displaydev`, Mesa (the renderer) → `gpu` | |
| USB devices over the bridge (Bluetooth, cameras) | |
| Restart, device reset, suspend | |

## Testing

`cargo xtask linux` builds the guest's kernel (on Linux; Windows needs WSL
for it); every image built afterwards includes the guest. Under QEMU, Veda
runs its guests on the processor's VMX, which KVM gives it nested
(`kvm_intel nested=1`); scripts that need it say `requires drivervm` and
boot with `drivervm` on the kernel command line (`drivervm.run=PROGRAM`,
`drivervm.poweroff`, `drivervm.memory=MIB`, `drivervm.cpus=N`,
`drivervm.devices=VID:DID,...`). Scripts that give the guest a device
say `iommu` too (`--iommu` for `cargo xtask run`): QEMU's machine then has
an Intel IOMMU that remaps interrupts. The logic that touches no hardware
(`vhv`: the APIC, `cpuid`, the boot protocol, a function's configuration
space; `viommu`: the DMAR table, the units' structures) is unit-tested on
the host.
