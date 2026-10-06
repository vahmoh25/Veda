# Cirrus Logic amplifier firmware

Firmware for the CS35L41 speaker amplifiers' DSP, from the Linux firmware
collection (`linux-firmware`, https://gitlab.com/kernel-firmware/linux-firmware).
The files are unmodified and are redistributed under Cirrus Logic's licence
(`LICENSE.cirrus`, installed beside them), which allows their use only with
Cirrus Logic devices. Veda's `lpss-spi` driver loads them into the amplifiers,
as Linux's `cs35l41_hda` does; nothing else reads them.

They are installed at `/system/firmware/cirrus/`, under the names Linux looks
them up by:

| File | What it is | linux-firmware path |
|------|------------|---------------------|
| `cs35l41-dsp1-spk-prot-10431f62.wmfw` | Speaker protection (CSPL) for the DSP, v29.63.1 of the algorithm | `cirrus/cs35l41/v6.61.1/halo_cspl_RAM_revB2_29.63.1.wmfw` (Linux installs it under this name as a link) |
| `cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.bin`, `-spkid1-l0.bin` | ASUS's tuning of the left speaker of the Zenbook Pro 16X (UX7602), by speaker id | same names under `cirrus/` |
| `cs35l41-dsp1-spk-prot-10431f62-spkid0-r0.bin`, `-spkid1-r0.bin` | The right speaker's | same names under `cirrus/` |

`10431f62` is the board's subsystem id (its ACPI `_SUB`). The speaker id comes
from a GPIO pin and tells which maker's speakers are fitted; for this board both
ids' files are the same.

SHA-256:

```
c11cb3e25825c673a955a3519370ad77fbcd8bcf68808e8e34140a70caefcec4  cs35l41-dsp1-spk-prot-10431f62.wmfw
f23e2b8a99a51858b47d13e804b656de2540e5ce961a088721988402f05c7e77  cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.bin
73596c413e3ace2e44706771af9188dd866ed3a6f188814123e73a24eb82fdb6  cs35l41-dsp1-spk-prot-10431f62-spkid0-r0.bin
f23e2b8a99a51858b47d13e804b656de2540e5ce961a088721988402f05c7e77  cs35l41-dsp1-spk-prot-10431f62-spkid1-l0.bin
73596c413e3ace2e44706771af9188dd866ed3a6f188814123e73a24eb82fdb6  cs35l41-dsp1-spk-prot-10431f62-spkid1-r0.bin
ff6e5ab98d3e3c50d9e750580059bc23a0663a5613659828d0536db44c38f4e0  LICENSE.cirrus
```

Another board's files are added the same way: the `.wmfw` under the name its
link has in linux-firmware's `WHENCE`, and its `.bin` files as they are.
