#!/usr/bin/env python3
"""Render the original Q2 demo audio with NumPy, Flite and FFmpeg."""
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory

import numpy as np

OUTPUT = Path(__file__).resolve().parent
RATE = 44_100
TAU = 2 * np.pi

# Each row specifies tempo, lead instrument, rhythm, four chord voicings and
# an original four-bar melody. Melody events are (beat, MIDI note, length).
ARRANGEMENTS = {
    "tidal": (66, "glass", "air", [(47, 54, 57, 61, 66), (45, 52, 56, 59, 64),
        (42, 49, 52, 57, 61), (45, 52, 57, 59, 64)], [
        (1, 73, 1.5), (3, 69, .8), (5.5, 68, 2), (8.5, 69, 1.8),
        (11, 66, 1.2), (13, 64, 2), (15, 69, 1)]),
    "northline": (104, "strings", "indie", [(43, 55, 59, 62, 69), (38, 54, 57, 62, 66),
        (40, 55, 59, 64, 67), (36, 52, 55, 60, 64)], [
        (0, 67, .7), (1.5, 71, .6), (2.5, 74, .9), (4, 69, 1), (5.5, 66, .7),
        (7, 62, .8), (8, 64, .9), (9.5, 67, .7), (11, 71, .8),
        (12, 72, 1), (13.5, 67, .7), (15, 64, .8)]),
    "afterhours": (118, "pulse", "club", [(41, 53, 56, 60, 63), (37, 49, 53, 56, 60),
        (44, 56, 60, 63, 67), (39, 51, 55, 58, 62)], [
        (0, 72, .4), (.75, 72, .3), (1.5, 68, .5), (2.5, 65, .6), (4, 68, .5),
        (5.5, 72, .8), (7.25, 65, .4), (8, 75, .5), (9.5, 72, .7),
        (11, 67, .6), (12, 70, .7), (13.5, 67, .5), (15, 63, .6)]),
    "sundial": (77, "piano", "air", [(36, 55, 59, 62, 64), (45, 55, 59, 60, 64),
        (41, 53, 57, 60, 67), (43, 53, 57, 62, 65)], [
        (0, 72, .7), (1, 76, .7), (2.5, 74, 1), (4, 71, 1.1), (6, 67, .9),
        (8, 69, .7), (9, 72, .7), (10, 77, 1.35), (12.5, 74, 1.15), (15, 67, .8)]),
    "softsignal": (93, "glass", "break", [(42, 54, 57, 61, 64), (38, 54, 57, 61, 66),
        (45, 57, 61, 64, 68), (40, 55, 59, 63, 66)], [
        (.5, 78, .45), (1.25, 76, .55), (2.75, 73, .7), (5, 69, 1),
        (6.5, 73, .7), (8, 76, .65), (9.5, 81, .55), (10.75, 76, .65),
        (12.5, 75, 1), (14.25, 71, 1)]),
    "fieldnotes": (89, "strings", "folk", [(38, 50, 57, 62, 66), (43, 55, 59, 62, 69),
        (47, 54, 59, 62, 66), (45, 52, 57, 61, 64)], [
        (0, 66, .58), (1, 69, .8), (2, 74, .8), (3, 76, .8), (4, 74, 1),
        (6, 71, 1.3), (8, 66, .6), (9, 71, .8), (10.5, 74, 1),
        (12, 73, .9), (13.5, 69, .7), (15, 64, .8)]),
    "copper": (110, "reed", "swing", [(38, 53, 57, 60, 64), (43, 53, 57, 59, 64),
        (36, 52, 55, 59, 62), (33, 49, 55, 58, 64)], [
        (.67, 65, .9), (2, 69, .45), (2.67, 72, .3), (3.25, 76, .6),
        (4.67, 77, .55), (5.67, 74, .6), (7, 71, .9), (8, 72, 1.2),
        (9.67, 67, .7), (10.67, 64, .4), (11.33, 62, .55), (12.67, 70, .55),
        (13.67, 67, .65), (14.67, 61, .35), (15.33, 64, .6)]),
    "pinksky": (123, "bright", "pop", [(44, 56, 60, 63, 67), (39, 55, 58, 63, 65),
        (41, 53, 56, 60, 67), (37, 53, 56, 60, 63)], [
        (0, 75, 1.1), (1.5, 72, .65), (2.5, 67, .55), (3.5, 70, .4),
        (4, 77, 1.2), (6, 75, .55), (7, 70, .55), (8, 72, .95),
        (9.5, 75, .55), (10.5, 80, 1), (12, 77, 1), (13.5, 75, .6),
        (14.5, 72, 1)]),
    "radio": (99, "piano", "break", [(45, 52, 55, 60, 64), (38, 54, 60, 64, 66),
        (43, 53, 57, 59, 64), (36, 52, 55, 59, 62)], [
        (0, 72, .75), (1.25, 76, .45), (2.5, 79, .8), (4.5, 78, .8),
        (6, 76, .6), (7.25, 72, .5), (8, 71, .6), (10, 69, .75),
        (12, 67, .5), (13, 71, .7), (14.5, 74, 1)]),
}

SPOKEN = [
    ("slt", "The tide had taken the road again. On the first page of her atlas, Mina drew a narrow blue line where the road had been. Then, across the water, a white lantern blinked twice. The island that carried it wasn't on any chart."),
    ("rms", "At the end of each evening, I walk the same four blocks. Tonight a light stayed on above the bakery. The window was open just enough to hear somebody laughing. I had passed this building a hundred times, and never once looked up."),
    ("awb", "On this episode of Field Notes, we're going under the city. Behind a grey door beside the market, there are thirty stone steps. At the bottom, the street sounds disappear. What you hear instead is water, moving through a channel laid a hundred years ago."),
    ("awb", "This is Field Notes. It's nearly midnight, and the last train is waiting at platform four. The conductor has checked the rear lights and raised her hand. Today we're riding to the end of the line to meet the people who get the rest of us home."),
    ("slt", "Welcome to The Listening Hour. On the oldest map, the bend in the river lies almost a mile east of where we're standing. The river has moved, but the towns still tell its story. We begin with a front porch, and three little marks cut into a door frame."),
    ("rms", "My grandmother dated her letters by what happened in the orchard. The first came in the month the plums bloomed. The second arrived when the willow leaves began to yellow. I knew the third would be an invitation. I had already packed my bag."),
]


def harmonic(phase, weights):
    return sum(weight * np.sin((index + 1) * phase) for index, weight in enumerate(weights))


def voice(name, note, t):
    phase = TAU * (440 * 2 ** ((note - 69) / 12)) * t
    if name == "pad":
        return (.52 * np.sin(phase + .25 * np.sin(TAU * .71 * t))
                + .28 * np.sin(phase * 1.004) + .12 * np.sin(2 * phase))
    if name == "glass":
        return np.sin(phase + 1.7 * np.exp(-4 * t) * np.sin(phase * 2.003)) * np.exp(-1.35 * t)
    if name == "strings":
        return (harmonic(phase, [1, .41, .18, .08, .04]) * np.exp(-3.3 * t)
                + .19 * harmonic(phase * 1.003, [1, .48, .2]) * np.exp(-5.8 * t))
    if name == "piano":
        return (np.sin(phase) * np.exp(-1.65 * t) + .34 * np.sin(phase * 2.003) * np.exp(-3.8 * t)
                + .18 * np.sin(phase * 3.013) * np.exp(-6 * t))
    if name == "reed":
        bent = phase + .48 * np.sin(TAU * 5.2 * t) * (1 - np.exp(-4 * t))
        return harmonic(bent, [.9, .35, .22, .09, .045])
    if name == "bass":
        return harmonic(phase, [1, .34, .11, .04]) * np.exp(-1.9 * t)
    brightness = 1 if name == "bright" else .63
    return (.58 * np.sin(phase) + brightness * harmonic(phase * 1.002, [.38, .35, .22, .13, .06])
            * (.41 + .59 * np.exp(-5.6 * t)))


class Mix:
    def __init__(self, bpm, seed):
        self.beat = 60 / bpm
        self.samples = round(16 * self.beat * RATE)
        self.signal = np.zeros((self.samples, 2))
        self.rng = np.random.default_rng(seed)

    def add(self, samples, beat, level, pan, attack, release):
        t = np.arange(len(samples)) / RATE
        envelope = np.minimum(t / attack, 1) * np.minimum(t[::-1] / release, 1)
        stereo = (samples * envelope * level)[:, None] * np.array([(1 - pan) ** .5, pan ** .5])
        start = round(beat * self.beat * RATE) % self.samples
        front = min(len(samples), self.samples - start)
        self.signal[start:start+front] += stereo[:front]
        self.signal[:len(samples)-front] += stereo[front:]

    def note(self, style, beat, note, length, level, pan=.5):
        t = np.arange(round(length * self.beat * RATE)) / RATE
        attack = {"pad": .30, "reed": .041, "bright": .028}.get(style, .008)
        release = .44 if style == "pad" else min(.13, t[-1] / 2)
        self.add(voice(style, note, t), beat, level, pan, attack, release)

    def drum(self, style, beat, level, pan=.5):
        duration = {"kick": .29, "snare": .23, "brush": .28, "hat": .075, "open": .22}[style]
        t = np.arange(round(duration * RATE)) / RATE
        if style == "kick":
            wave = np.sin(TAU * (48 * t + 2.1 * (1 - np.exp(-29 * t)))) * np.exp(-15 * t)
        else:
            noise = self.rng.normal(0, .5, len(t))
            high = np.diff(noise, prepend=0)
            if style in {"snare", "brush"}:
                wave = (.68 * noise + .25 * np.sin(TAU * 177 * t) * np.exp(-18 * t)) * np.exp(-12 * t)
                if style == "brush":
                    wave *= np.minimum(t / .032, 1)
            else:
                wave = high * np.exp((-49 if style == "hat" else -16) * t)
        self.add(wave, beat, level, pan, .0015, .024)


def music(arrangement, seed):
    bpm, lead, rhythm, chords, melody = arrangement
    mix = Mix(bpm, seed)
    for bar, (root, *chord) in enumerate(chords):
        beat = bar * 4
        if rhythm == "swing":
            for step, tone in enumerate([root, root + 12, root + 7, root + 4]):
                mix.note("bass", beat + step, tone, .75, .20)
            for step, gain in ((.67, .08), (2.67, .1)):
                for index, tone in enumerate(chord):
                    mix.note("piano", beat + step + .023 * index, tone, 1.2, gain, .25 + .15 * index)
        else:
            for index, tone in enumerate(chord):
                pan = .24 + index * .17
                strength = .115 if rhythm == "air" else .054 if rhythm == "club" else .072
                mix.note("pad", beat - .22, tone, 4.55, strength, pan)
                if rhythm in {"folk", "indie", "break", "pop"} or lead == "piano":
                    steps = (index * .5, 2.5 + index * .33) if rhythm == "folk" else (index * .33,)
                    for step in steps:
                        backing = "strings" if rhythm in {"folk", "indie"} else "piano"
                        mix.note(backing, beat + step, tone, 1.65, .12, pan)
            bass_steps = {"air": (0,), "indie": (0, 2.5), "folk": (0, 2),
                          "club": (.5, 1.5, 2.5, 3.5), "break": (0, 1.75, 3.25),
                          "pop": (0, 1.5, 2.5, 3.5)}[rhythm]
            for index, step in enumerate(bass_steps):
                tone = root if index < 2 or rhythm == "club" else root + 12
                mix.note("bass", beat + step, tone, .66 if len(bass_steps) > 2 else 1.7, .20)

        if rhythm == "air":
            continue
        kicks = {"club": (0, 1, 2, 3), "pop": (0, 1.5, 2, 3), "indie": (0, 1.5, 2.75),
                 "folk": (0, 2), "swing": (0, 2), "break": (0, 1.75, 2.5)}[rhythm]
        for step in kicks:
            level = .29 if rhythm in {"club", "pop"} else .11 if rhythm in {"swing", "folk"} else .22
            mix.drum("kick", beat + step, level)
        for step in (1, 3):
            brushed = rhythm in {"swing", "folk"}
            mix.drum("brush" if brushed else "snare", beat + step, .12 if brushed else .22, .46)
        hat_steps = (0, .67, 1, 2, 2.67, 3) if rhythm == "swing" else (.5, 1.5, 2.5, 3.5)
        for index, step in enumerate(hat_steps):
            style = "open" if index == 2 and rhythm in {"club", "pop", "swing"} else "hat"
            mix.drum(style, beat + step, .075 + .027 * (index % 2), .64)

    for index, (beat, pitch, length) in enumerate(melody):
        mix.note(lead, beat, pitch, length, .22 + .03 * (index % 3), .46)
        if rhythm != "swing":
            echo = .74 if rhythm == "air" else .375
            mix.note(lead, beat + echo, pitch, length, .057, .76)
            mix.note(lead, beat + echo * 2, pitch, length, .022, .20)

    # A short stereo room response also wraps around the loop seam.
    room = mix.signal.copy()
    for seconds, gain in ((.037, .13), (.089, .08), (.143, .055)):
        mix.signal += gain * np.roll(room[:, ::-1], round(seconds * RATE), axis=0)
    # Browsers may expose a little encoder padding when looping an MP3.
    # Ease to zero around the boundary so that padding cannot introduce a click.
    edge = round(.008 * RATE)
    ramp = (.5 - .5 * np.cos(np.linspace(0, np.pi, edge)))[:, None]
    mix.signal[:edge] *= ramp
    mix.signal[-edge:] *= ramp[::-1]
    rms = np.sqrt(np.mean(mix.signal ** 2))
    return .93 * np.tanh(.14 * mix.signal / rms)


def ffmpeg(source_args, output_args, target, data=None):
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *source_args,
                    "-map_metadata", "-1", "-c:a", "libmp3lame", *output_args, target],
                   input=data, check=True)


def main():
    for seed, (key, arrangement) in enumerate(ARRANGEMENTS.items()):
        samples = music(arrangement, seed).astype("<f4")
        ffmpeg(["-f", "f32le", "-ac", "2", "-ar", str(RATE), "-i", "pipe:0"],
               ["-b:a", "80k"], OUTPUT / f"{key}.mp3", samples.tobytes())

    with TemporaryDirectory(dir=OUTPUT.parents[1] / "build") as tmp:
        source = Path(tmp) / "speech.wav"
        for index, (speaker, passage) in enumerate(SPOKEN):
            source.unlink(missing_ok=True)
            subprocess.run(["flite", "-voice", speaker, "-t", passage, "-o", source], check=True)
            ffmpeg(["-i", source], ["-af", "highpass=f=85,lowpass=f=7600,loudnorm=I=-18:TP=-2:LRA=8,"
                                    "adelay=110,apad=pad_dur=0.33,asetpts=N/SR/TB", "-ar", "44100", "-b:a", "56k"],
                   OUTPUT / f"spoken-{index}.mp3")


if __name__ == "__main__":
    (OUTPUT.parents[1] / "build").mkdir(exist_ok=True)
    main()
