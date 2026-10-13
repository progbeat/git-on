#!/usr/bin/env python3
"""Convert a recorded GIF to WebP, preserving decoded pixels and timing."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile

from PIL import Image, features


def timeline(path):
    result = []
    with Image.open(path) as image:
        dimensions, loop = image.size, image.info['loop']
        first = last = None
        for index in range(image.n_frames):
            image.seek(index)
            rgb = image.convert('RGB')
            digest = hashlib.sha256(rgb.tobytes()).hexdigest()
            first = digest if first is None else first
            last = digest
            duration = image.info['duration']
            if result and result[-1][0] == digest:
                result[-1][1] += duration
            else:
                result.append([digest, duration])
    return dimensions, loop, result, first == last


def convert(source, destination):
    if destination.exists() or destination.is_symlink():
        raise ValueError(f'Refusing to overwrite {destination}')
    frames, durations = [], []
    with Image.open(source) as image:
        loop = image.info['loop']
        for index in range(image.n_frames):
            image.seek(index)
            frames.append(image.convert('RGB').copy())
            durations.append(image.info['duration'])
    settings = dict(lossless=True, quality=100, method=6,
                    minimize_size=True, allow_mixed=False)
    with tempfile.TemporaryDirectory(prefix='.git-on-webp-', dir=destination.parent) as temporary:
        staged = Path(temporary) / 'animation.webp'
        with staged.open('xb') as output:
            frames[0].save(output, 'WEBP', save_all=True,
                           append_images=frames[1:], duration=durations,
                           loop=loop, **settings)
        before, after = timeline(source), timeline(staged)
        if before != after:
            raise ValueError('Decoded pixels, timing, dimensions or loop changed')
        result = dict(source_gif_bytes=source.stat().st_size,
                      webp_bytes=staged.stat().st_size,
                      webp_sha256=hashlib.sha256(staged.read_bytes()).hexdigest(),
                      dimensions=list(after[0]), loop=after[1],
                      duration_ms=sum(item[1] for item in after[2]),
                      lossless_rgb_and_timeline=True, first_last_identical=after[3],
                      encoder=f'Pillow {Image.__version__}; libwebp {features.version("webp")}',
                      settings=settings)
        try:
            os.link(staged, destination, follow_symlinks=False)
        except FileExistsError:
            raise ValueError(f'Refusing to overwrite {destination}') from None
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gif', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(convert(args.gif, args.output), indent=2))
