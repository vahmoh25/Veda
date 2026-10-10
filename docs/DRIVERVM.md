# The driver VM

Veda drives only what it cannot do without — the disks it starts from —
and takes everything else from Linux: GPUs and displays, audio, networks,
Wi-Fi, Bluetooth, USB, input, cameras, and the thousands of devices Linux
has drivers for ([Status](#status) says how far that has come). Linux runs
in a virtual machine of Veda's own, *the driver VM*, which gets those
devices; its drivers serve Veda's services through the same protocols
Veda's own drivers speak. A driver that crashes, hangs or is compromised
takes down a virtual machine that Veda restarts, never Veda.

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Veda: services (audio, compositor, netd, wlan…)   native drivers: the    │
 │       speak audiodev, displaydev, netdev, gpu…     disks                 │
 │                     │                                                    │
 │             drivervm (the monitor: memory, processors, hypercalls,       │
 │             the bridge, the registry the guest sees, devices it gets)    │
 ├─────────────────────│──────────────────────────────────────────────────── │
 │ vkernel: Guest (EPT) · Vcpu (VMX, x2APIC) · IOMMU domains · interrupts   │
 ╞═════════════════════│════════════════════════════════════════════════════╡
 │ Linux (the guest)   │                                                    │
 │   programs: Veda's drivers for Linux (vrt over /dev/veda): ALSA →        │
 │   audiodev, KMS → displaydev, Mesa → gpu, nl80211 → wlanphy, …          │
 │   kernel: Linux's drivers (i915/xe, amdgpu, snd-hda/SOF, iwlwifi, mt76,  │
 │   btusb…) + the Veda platform (console, bridge, PCI, processors)         │
 └──────────────────────────────────────────────────────────────────────────┘
        devices given to the guest: their BARs in its memory, their DMA
        through the IOMMU into its memory only, their interrupts remapped
```

## Why

Veda's own drivers covered what a PC needs to start and be used: storage,
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
   is given away. A USB controller is given whole too, with every device
   on it: Linux drives the hubs, the keyboards and mice (a laptop's
   built-in keyboard is often one), the network and Bluetooth adapters.

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
   crashes, the process ends, and `devmgr` resets the devices and gives
   them to a new one. Veda's services already handle a driver going away
   (calls have timeouts, drivers attach again).

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
| Device manager | `services/devmgr` | which devices go to the driver VM (all but Veda's own), their `pcidev` channels, resetting them and starting the driver VM again |
| Monitor | `services/drivervm` | the machine, its hypercalls, its PCI functions, the bridge, the narrowed registry |
| Guest kernel | `ports/linux` | Linux with the Veda platform: `arch/x86/kernel/cpu/veda.c`, `arch/x86/pci/veda.c`, `drivers/tty/hvc/hvc_veda.c`, `drivers/virt/veda/bridge.c` |
| Guest runtime | `lib/rt/src/guest.rs` | `vrt`'s system calls through `/dev/veda`; watches |
| Guest programs | `guest/` | `init`; Veda's drivers for Linux (`input`, `alsa`, `net`, `wifi`, `kms`) and its renderer (`renderer`: C and Rust); `airlink` (QEMU's virtual radio as Linux's); `bridgetest`, `pcitest`; what they share (`sys`: system calls, network interfaces; `netlink`) |
| Build | `xtask/src/linux.rs`, `ports/linux/build.sh` | `cargo xtask linux`: the kernel, a toolchain for the guest's programs in C and C++, Mesa; the initramfs, with the firmware of `ports/linux/firmware.txt`; the image's `linux/` |

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

Every PCI function goes to the driver VM but those Veda keeps: the disks
it starts from (storage controllers, which are none of Linux's
business), the platform's own functions (bridges, system peripherals,
the SMBus, and the serial bus controllers, one of which holds the
firmware's flash), and the devices Veda still has drivers for (sound
cards, and the SPI controller of a laptop's speaker amplifiers: they
come with the firmware's descriptions of what is wired to them, see
[Status](#status)). The boot options give it more
(`drivervm.devices=VID:DID,...`: a sound card, in tests) or none
(`drivervm=off`). It starts when the machine can give it devices: its
processors run virtual machines (VMX with EPT), an IOMMU confines the
devices (the kernel tells `devmgr` both, `vabi::platform`), and the
system image has its Linux; `drivervm` starts it even without devices
(tests). `devmgr` starts no driver of Veda's for a function the driver VM
gets, and hands the driver VM its `pcidev` channel, as it hands a driver
its device. Through it the monitor gets what giving the function away
takes, and nothing more: its configuration space, its BARs, its MSIs,
and a resource that names it (`resource_kind::PCI`, by requester id),
which only the driver VM's functions have. A function the monitor cannot
give (one the firmware keeps memory for, below) it lets go of at once:
`devmgr` keeps it, and neither resets it when the driver VM ends nor
offers it to the next.

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
on a PC find it. The kernel's command line says where each function's
memory is on the host (`veda.device=00:01.0,0:0x80000000,...`): a driver
in the guest names the host's memory it drives so, to services that know
the host's addresses (the compositor, where the firmware's framebuffer
was).

**Errors.** The errors a function signals become the host's system
errors, which a PC may turn into NMIs, and an NMI stops Veda: a given
function reports none. The monitor turns its SERR# enable and its PCI
Express error reporting off before the guest runs, and they stay off,
whatever the guest writes.

What a function needs for this, and the limits for now: MSIs (or MSI-X),
without which it has no interrupts in the guest (QEMU's 82540EM, its
`e1000`, has none; a PCI Express function always has them), memory BARs
of whole pages (an I/O BAR it also has is not given: Linux's drivers of
PCI Express functions use their memory BARs), and its first 256 bytes of
configuration space (`devmgr` reaches no more yet); no memory the
firmware keeps for it (an RMRR: such a device stays the host's); the
guest's memory is at most 3 GiB. Under QEMU, a virtio function's DMA goes
through the IOMMU only if the function says so (`iommu_platform`): xtask
gives QEMU's virtio functions that, modern ones only, as a PC's are
behind its IOMMU, and Veda's virtio drivers accept it
(`VIRTIO_F_ACCESS_PLATFORM`).

### Veda's drivers for Linux

A driver for Linux is a program of the guest's, written as a Veda driver
is, against Linux's device interface instead of the hardware. The guest's
`init` starts the ones its devices need (by PCI class; `input` always),
and again if they end. A driver waits on Linux's files and Veda's objects
at once: a *watch* (`vrt::guest::watch`) is a file that is readable while
a handle's signals are, so one `poll` takes a card's socket, a channel of
Veda's and a ring's wake-up event.

**Input** (`guest/input`). The keyboards, mice, tablets and touchscreens
Linux drives, on USB, virtio or wherever its drivers find them, are input
devices of Veda's window system: the driver reads Linux's event devices
(evdev), each grabbed, so that nothing of Linux's acts on what is typed,
and sends what a device reports at once (up to its `SYN_REPORT`) to the
compositor's `input` service in one batch, as Veda's own drivers do: keys
and buttons by evdev's codes, which are Veda's; relative motion and
wheels; where an absolute pointer is, as a fraction of its range (a
touchscreen's touch is the left button). Devices come and go with the
kernel's uevents; keys a device held when it went, or whose releases
Linux dropped, are let go. The keyboards' LEDs show Num Lock (the keypad
types digits) and Caps Lock as the window system has it. Touchpads, whose
positions are the pad's rather than the screen's, are not used yet. Linux
acts on no key itself: it has no console, and SysRq is only
`/proc/sysrq-trigger`'s.

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
A card that records (its first capture device) is the device's input too:
a thread of its own reads the card a period at a time into the input
ring, stamped with the card's count as playback is, from when the audio
service opens the input until it closes it.

**Network** (`guest/net`). Each of Linux's Ethernet cards (a PCI or USB
device's interface) is attached to Veda's network service as a network
device (`netdev`), as Veda's own drivers attach theirs: its frames go
between a raw packet socket on the interface (every frame it receives,
multicast too; none it sends) and the device's ring, a copy each way, and
its carrier is the link's. Linux has no IP stack in the driver VM: it
moves frames and answers nothing on the network itself.

**Wi-Fi** (`guest/wifi`). Each of Linux's Wi-Fi radios is offered to
Veda's Wi-Fi service as a *managed* radio ([Networking](NETWORKING.md)):
Linux's 802.11 stack is its MLME, on the service's commands, through
nl80211 — it scans, sends the authentication and association frames,
retries, encrypts, and watches the link; the service decides what to
join, does SAE and the key handshakes, and gives Linux the session keys
only. There is no supplicant in the guest. The driver's netlink socket
owns the connection and its control port, over which EAPOL comes and
goes: if the driver ends, Linux leaves the network. Data is Ethernet, as
for `net`. Under QEMU, `airlink` (`guest/airlink`) makes the virtual radio
(the virtio-serial port airsim is on) a radio of Linux's own, one of
`mac80211_hwsim`'s, and is its medium; the kernel's command line has it
make none itself.

**Display** (`guest/kms`). A display Linux drives (KMS) is attached to
Veda's compositor as the screen it flips (`displaydev`), as Veda's own
display drivers attach theirs: the one whose memory holds the firmware's
framebuffer (a machine may have more GPUs with outputs), at the host's
address of one of its BARs, which the monitor writes on Linux's command
line (`veda.device=`) and the compositor tells (`displaydev::screen`).
Linux's driver sets the mode itself — the compositor's screen — from the
compositor's first frame on. The compositor draws into pictures of Veda's
memory, which the driver hands it, and Linux shows them as they are:
imported as dma-bufs (the bridge maps each into the guest; a display engine
reads it through the IOMMU, a display without one, such as QEMU's standard
VGA, copies it into its own memory as it updates). A driver that cannot
import gets each picture copied into a buffer of its own before the flip.
Flips are page flips at the vertical blank; their events' timestamps, on
Linux's monotonic clock, become Veda's (both follow the TSC), so the
compositor times its frames by the display's real blanks.

**GPU** (`guest/renderer`). Veda's applications reach the GPU through
`gpu`, the renderer's protocol, in which their OpenGL ES (`vgl`) sends
virgl's commands; the renderer serves it from the guest, on Mesa's
Gallium driver of the GPU over the GPU's Linux driver, on its render node:
iris on i915 or xe, virgl on virtio-gpu (QEMU's, whose virglrenderer
renders on the host's GPU), softpipe when asked (`drivervm.renderer=softpipe`,
for tests). Each connection gets a context and a block of Veda's memory
shared with it, which the renderer maps through the bridge; the decoder
(`decoder/`, C) checks every command before Gallium's calls carry it out,
and a context's fences signal through the shared memory and an event.
The compositor's pictures reach a GPU's driver as dma-bufs of their VMOs
(the bridge's, as for `kms`), which it renders into through the IOMMU, so
the GPU composes the screen in place; softpipe renders into them mapped,
and virgl not at all (its host renders into memory of its own), so that
under QEMU the compositor composes on the processor. The program is
Mesa's build: the decoder and the drivers around the Rust half, which
serves the protocol (Rust and C++ linked statically, on musl, with the
guest's toolchain). GPU memory is the guest's, which Linux's driver
allocates.

**USB**. USB controllers go to the driver VM whole (xHCI's, which PCs have
had since about 2012), and Linux drives them, their hubs and every device
on them as on any PC: keyboards, mice and tablets for `input`, network
adapters for `net` (CDC Ethernet and NCM, Realtek's and ASIX's), Bluetooth
adapters with Linux's Bluetooth stack (and their firmware), which no
service of Veda's uses yet. Disks on USB it leaves alone: disks are
Veda's, which has no driver for them yet. When the driver VM ends,
`devmgr` resets the controller with its other devices, and the next driver
VM finds the devices again.

**Firmware.** The initial RAM file system carries the firmware Linux's
drivers load (`/lib/firmware`): the files `ports/linux/firmware.txt`
lists, from a pinned release of linux-firmware (and cfg80211's regulatory
database, from wireless-regdb), each checked against its SHA-256. A device
whose firmware is not listed does not work in the driver VM; its driver
and its firmware go in together. Loading firmware from Veda's file system
when a device asks, rather than carrying all of it, comes when the list
grows.

### The bridge

| Guest | Kernel of the guest | Monitor |
|-------|---------------------|---------|
| `vrt::object::Channel::write(bytes, handles)` | `ioctl(VEDA_IOC_CHANNEL_WRITE)`: copies the buffers in, checks the file holds the handles | moves the guest's handles out of its table, writes the message |
| `wait_many(items, deadline)` | `VEDA_IOC_WAIT`: finished at once, or sleeps until the ring says | polls; or keeps the wait for its waiter thread, which writes the result and raises the callback vector |
| `Mapping::new(vmo)` | `VEDA_IOC_VMO_MAP`, then `mmap` | maps the VMO into the window of guest-physical memory |
| `vrt::guest::watch(handle, signals)` | `VEDA_IOC_WATCH`: a file whose `poll` makes a wait, and reads its result | an ordinary wait (cancelled when the file closes) |
| `vrt::guest::dmabuf(vmo, offset, len, writable)` | `VEDA_IOC_VMO_DMABUF`: a dma-buf of the window range, DMA addresses for importers' devices or mapped for the processor | maps the VMO into the window, as for a program; unmaps it when the dma-buf goes |

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
   reaches 99% on Qualcomm's GPUs. Veda cuts at its own driver protocols,
   which are asynchronous and share memory (decision 1). For GPUs that is
   `gpu`: Veda's OpenGL ES (`vgl`) already sends the renderer its
   commands (virgl's, close to Gallium's), so the API crosses one boundary
   either way, and the renderer runs in the guest, on Mesa's own driver of
   the GPU over its kernel driver, with nothing between them. Forwarding
   the kernel interface instead would have kept Mesa in Veda and needed
   Veda's work for every family of GPUs (its kernel interface, its Mesa
   driver ported); this way a GPU Linux and Mesa drive needs none.
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
   the device (Xoar). A guest's devices reach nothing once it ends (what a
   device goes on doing is refused, and said once); `devmgr` resets each
   (a function-level reset, or through D3hot, or neither, which it says)
   and starts a new guest: a network card is usable again about two
   seconds after Linux crashed. Devices that need resets of their own,
   and a guest that hangs rather than crashes, are later.
9. **Suspend, device by device.** Qubes unloads its PCI drivers before
   suspending; Ghaf picks a policy per VM. Later.
10. **Fixed memory, fast boot.** Passthrough rules out ballooning
    (Qubes); Firecracker reaches its guest's init in 125 ms. The driver
    VM's memory is one committed VMO, and Linux reaches its init in about
    0.1 s.
11. **Narrow, typed control.** Genode's Wi-Fi driver takes a declarative
    configuration and reports access points; Ghaf filters the D-Bus it
    forwards from its network VM. Veda's own protocols (`wlanphy` and its
    managed radios' `wlanmlme_ctl`, `netdev`, `audiodev`) are that
    boundary already: Linux's Wi-Fi stack is given session keys, never a
    password, and decides nothing.
12. **Own the presentation clock.** No system found exports a GPU VM's
    display to another system's compositor with its real vertical blank:
    virtio-gpu's emulated one has been wrong, Xen's display protocol is
    fixed at 60 Hz. `displaydev` carries real flip events and timestamps:
    `kms` reports KMS's page flips with the blank they happened at, on
    Veda's clock.
13. **Few driver VMs to start with.** IOMMU groups put devices together,
    and combining several driver VMs is still hard elsewhere (seL4's
    libvmm). One driver VM for now; nothing in the design assumes one.

## Status

| Phase | State |
|-------|-------|
| Linux runs in Veda: hypervisor, monitor, platform, console, SMP, power | done (`tests/ui/drivervm.vts`) |
| The bridge: Veda's IPC in the guest | done (`tests/ui/drivervm-bridge.vts`) |
| Devices: IOMMU (DMA and interrupt remapping), PCI given to the guest | done (`tests/ui/iommu.vts`, `tests/ui/drivervm-pci.vts`) |
| Audio: ALSA → `audiodev` (playback and recording) | done (`tests/ui/drivervm-audio.vts`, `drivervm-mic.vts`) |
| Network and Wi-Fi: Linux's cards → `netdev`, nl80211 → `wlanphy` (managed radios); Veda's own network and radio drivers gone | done (`tests/ui/drivervm-net.vts`, `e1000e.vts`, `drivervm-wifi.vts`, `drivervm-wifi-recovery.vts`, `wifi-connect.vts`, `network-failover.vts`) |
| Display: KMS → `displaydev`, the compositor's pictures shown as they are; Veda's own display drivers gone | done (`tests/ui/drivervm-display.vts`, `drivervm-display-restart.vts`, `drivervm-display-crash.vts`) |
| GPU: the renderer in the guest, on Mesa's Gallium drivers over Linux's (virgl under QEMU; iris; softpipe); Veda's own GPU drivers gone | done (`tests/ui/drivervm-gpu.vts`, `drivervm-renderer.vts`, `drivervm-compose.vts`); Intel's integrated GPUs next (below) |
| Input and USB: Linux's event devices → `input`; USB controllers whole (keyboards, mice, network and Bluetooth adapters); Veda's own USB and virtio input drivers gone | done (`tests/ui/usb-input.vts`, `drivervm-usb.vts`, `live-usb.vts`); touchpads later |
| Every device Veda does not keep goes to the driver VM, which starts with Veda | done (every script, `tests/ui/iommu.vts`) |
| Restart and device reset | done (`tests/ui/drivervm-restart.vts`); hangs, suspend later |
| The firmware's ties: its descriptions of devices for the guest (ACPI: I2C touchpads and touchscreens, a laptop's speaker amplifiers on SPI, GPIO pins), PS/2 keyboards, sound | next (below) |

**GPUs.** Linux's driver and Mesa's drive a GPU whole in the guest (lesson
1): the guest's Linux has i915 and virtio-gpu, and its Mesa iris, virgl
and softpipe; AMD's and NVIDIA's (amdgpu and radeonsi, nouveau and NVK)
come in with their firmware. Under QEMU the path is tested on the host's
GPU, through virtio-gpu given to the guest; what QEMU cannot give is a
PC's GPU. Intel's integrated GPUs come with ties to the firmware that the
guest needs too (lesson 7): memory reserved for them (an RMRR, which keeps
a device the host's for now), their OpRegion (the panel's description,
VBT) and stolen memory, and their place at 00:02.0. GPU memory is the
guest's: a GPU needs a driver VM with the memory for it.

**Still Veda's.** Sound cards (HD Audio, with a laptop's speaker
amplifiers on its SPI controller; AC'97; virtio-snd) and PS/2 keyboards
keep Veda's drivers until the guest gets the firmware's descriptions of
what they need: which amplifiers are on which bus, the GPIO pins wired to
them, the keyboard controller's ports and interrupts (the guest has no
ACPI of its own). A laptop's I2C touchpad and touchscreen need such
descriptions too, and nothing drives them yet.

## Testing

`cargo xtask linux` builds the guest's kernel, its toolchain and Mesa
(the first image built builds them too, where the machine has the tools);
every image includes the guest, and without it nothing but the disks,
sound and a PS/2 keyboard is driven. Under QEMU, Veda runs
its guests on the processor's VMX, which KVM gives it nested
(`kvm_intel nested=1`), and xtask's machine has an Intel IOMMU that
remaps interrupts, its virtio functions behind it, as a PC's are
(`--no-iommu` for `cargo xtask run` leaves it out): every script runs the
driver VM, and the GUI scripts need it (`cargo xtask test --ui` says why
when it cannot run). Its options are on the kernel command line:
`drivervm.run=PROGRAM`, `drivervm.poweroff`, `drivervm.memory=MIB`,
`drivervm.cpus=N`, `drivervm.devices=VID:DID,...` (devices Veda would
keep: a sound card), `drivervm=off`, and `drivervm.crash=SECONDS`, which
has Linux crash once, that long after it started, for the tests of the
restart. Wi-Fi's scripts give QEMU's machine the virtual radio's
virtio-serial function (`net wifi`), which `airlink` makes Linux's;
`usb net` a USB network adapter on its xHCI controller, and `input usb`
USB keyboards, mice and a tablet instead of the PS/2 keyboard and the
virtio tablet. The display and the GPU are QEMU's VGA and its virtio-gpu
(with `gpu on`; it renders on the host's GPU the firmware showed its
screen on, or the one `VEDA_RENDERNODE` names, such as
`/dev/dri/renderD129`), and `drivervm.renderer=softpipe` has the
renderer render on softpipe without a GPU. The renderer's decoder is tested on the host too, through vgl's
tests (on softpipe, and on Mesa's virgl over virglrenderer's test
server). The logic that touches no hardware
(`vhv`: the APIC, `cpuid`, the boot protocol, a function's configuration
space; `viommu`: the DMAR table, the units' structures) is unit-tested on
the host.
