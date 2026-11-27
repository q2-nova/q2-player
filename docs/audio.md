# Headphone output

On the Q2 the player writes through ALSA to `hw:Q2,0`, the four CS43131
DACs: S16_LE for sources of 16 bits or fewer and S32_LE otherwise. A change
of width at a track boundary drains the previous track first (AUDIO-004).
Requirements AUDIO-004 to AUDIO-008 are in the
[specification](specification.md#audio-and-sound). This page covers what the player sets in the codecs: volume,
the DAC filter and Headphone Power. The codec registers, board wiring and
measured power are in q2-os
[docs/hardware.md](https://github.com/q2-nova/q2-os/blob/main/docs/hardware.md#audio).
The codec driver is `sound/soc/codecs/cs43130.c` in
[q2-nova/linux](https://github.com/q2-nova/linux) (branch `q2/7.3`).

## Volume

`audio/mixer.rs` sets every DAC muted during platform start-up, before the
player exists, and reads each value back. The player then owns codec volume
on the headphone route: the volume is codec attenuation in the driver's
0.5 dB steps (raw 255 is 0 dB, raw 0 is mute), with any fraction the codec
grid cannot express applied in software (`audio/gain.rs`). Volume 100 is
0 dB and each step below it is 0.5 dB. A write is applied to all four DACs
and read back; on a mismatch or failure every DAC is muted and the PCM is
refused. EQ and ReplayGain stay in software. DAPM owns amplifier power and
the HP mute sequence.

## DAC filter

The DAC Filter setting's four choices map onto two codec controls on every
DAC:

| Setting | `PCM Filter Speed` | `PCM Phase Compensation` |
| --- | --- | --- |
| Sharp | Fast | off |
| Slow | Slow | off |
| Short Sharp | Fast | on |
| Short Slow (default) | Slow | on |

The stock firmware labels the last two "low delay". Reading low delay as
phase compensation follows the labels and is not checked against the
datasheet.

## Headphone Power

Settings › Sound › Headphone Power sets the codecs' full scale and class-H
rail mode through two controls per DAC, `DACn HP Full Scale` and
`DACn HP Class H`:

| Level | HP Full Scale | HP Class H | `OUT_FS` | `HV_EN` | `+1dB_EN` | `ADPT_PWR` | Codec attenuation |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Efficient (default) | 0.5 Vrms | Fixed VCP | 00 | 0 | 0 | 010 (±VCP) | 12 steps (6 dB) less than the volume asks, never below 2 steps (1 dB) |
| Standard | 1 Vrms | Adapt to Signal | 01 | 0 | 0 | 111 | as the volume asks |
| High | 1.73 Vrms | Adapt to Signal | 11 | 1 | 0 | 111 | as the volume asks |
| Maximum | 2 Vrms | Adapt to Signal | 11 | 1 | 1 | 111 | as the volume asks |

`OUT_FS` and `+1dB_EN` are HP_OUT_CTL_1 (0x080000) bits 5:4 and 0; `HV_EN`
and `ADPT_PWR` are CLASS_H_CTL (0x0B0000) bits 1 and 4:2. Maximum is the
stock firmware's setting (CLASS_H_CTL 0x1E, HP_OUT_CTL_1 0x31). With
`HV_EN` 0 the VP_LDO rail is 2.6 V, with 1 it is 3.0 V.

- **Loudness.** Efficient's 0.5 Vrms is 6.02 dB below Standard's 1 Vrms;
  with 12 fewer steps of codec attenuation each volume step plays at
  Standard's level up to Efficient's maximum. From volume 86 at unity track
  gain every step plays at that maximum, 7 dB below Standard's. High and
  Maximum are not compensated: they raise every step by 4.8 and 6.0 dB.
- **Why Efficient keeps 1 dB.** On fixed ±VCP rails (VCP 1.66 to 1.94 V)
  DS1155F2 table 4-2 rates the output to about −11 dB of 1.73 Vrms,
  0.49 Vrms at 16 Ω, so a 0.5 Vrms full scale has no datasheet margin.
  Efficient's loudest output is 0.45 Vrms.
- **When it is written.** The driver changes these registers only while the
  codec's HP stage is powered down and returns EBUSY otherwise; the card
  ignores the DAPM power-down delay, so the stage is down once the PCM
  closes. The player writes the level only with the headphone PCM closed. A
  change made while playing takes effect at the next pause: the queue is
  parked, the level written, and playback resumes from the parked audio.
  Leaving Fixed VCP sets the rail to adapt before raising the full scale;
  entering it lowers the full scale first. The driver refuses Fixed VCP with
  any full scale above 0.5 Vrms (EINVAL).
- **Older driver.** A codec driver without `HP Full Scale` has only
  `HP Output Level`: Low (1 Vrms, adapting) or High (1.73 Vrms). The player
  then applies Standard for Efficient and Standard, High for High and
  Maximum, and logs the substitution.
- **Saved settings.** A configuration from before the setting keeps an
  explicit High gain as High; otherwise it gets the default, Efficient.

### Evidence

| Fact | Level |
| --- | --- |
| Each level reads back on all four DACs' Full Scale and Class H controls before playback | device-verified |
| At volume 20 into a 600 Ω load (44.1 kHz FLAC, screen dark), Efficient and High against Standard differ by under 2 mW at the USB input in each block, with signs that change between blocks: no repeatable saving | device-verified |
| At the same operating point Maximum costs 13.28 mW more than Standard (blocks +13.04, +13.94, +12.86 mW; paired 95 % t interval +11.85 to +14.71 mW, three blocks) | device-verified |
| At digital silence with no load, the old Low and High levels differ by +0.16 mW (95 % interval −0.49 to +0.81) | device-verified |
| Whether an Efficient peak clips into 16 Ω | unverified |

The datasheet predicts that a lower full scale saves nothing at equal
loudness by itself; savings appear only where adapt mode would switch to the
VP_LDO rail, on loud passages at high output into a load. The power figures
come from an uncalibrated USB-input meter. Terminal voltage, acoustic
output, other loads and other volumes are not measured.
