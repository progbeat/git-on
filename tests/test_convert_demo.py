#!/usr/bin/env python3
"""Real lossless WebP conversion and output-preservation regressions."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from PIL import Image


SPEC = importlib.util.spec_from_file_location(
    "convert_demo", Path(__file__).resolve().parents[1] / "scripts/convert_demo.py")
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)


class ConversionArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "tiny.gif"
        frames = [Image.new("RGB", (8, 6), color) for color in [(12, 34, 56), (210, 180, 90)]]
        frames[0].save(self.source, "GIF", save_all=True, append_images=frames[1:],
                       duration=[70, 130], loop=2, disposal=2, optimize=False)
        self.source_bytes = self.source.read_bytes()
        self.destination = self.root / "tiny.webp"

    def attempt_conversion(self):
        try:
            return None, MOD.convert(self.source, self.destination)
        except Exception as error:
            return error, None

    def assert_refused(self, error):
        self.assertIsInstance(error, ValueError)
        self.assertIn("Refusing to overwrite", str(error))
        self.assertEqual(self.source.read_bytes(), self.source_bytes)

    def test_dangling_output_symlink_does_not_create_its_target(self):
        missing = self.root / "previously-missing-target.webp"
        self.destination.symlink_to(missing)
        error, _ = self.attempt_conversion()
        self.assertFalse(missing.exists(), "Converter followed the dangling link and created its target")
        self.assertTrue(self.destination.is_symlink())
        self.assertEqual(self.destination.readlink(), missing)
        self.assert_refused(error)

    def test_existing_regular_output_is_preserved(self):
        self.destination.write_bytes(b"previous conversion\n")
        error, _ = self.attempt_conversion()
        self.assertEqual(self.destination.read_bytes(), b"previous conversion\n")
        self.assert_refused(error)

    def test_existing_symlink_and_target_are_preserved(self):
        target = self.root / "other-conversion.webp"
        target.write_bytes(b"unrelated conversion\n")
        self.destination.symlink_to(target)
        error, _ = self.attempt_conversion()
        self.assertEqual(target.read_bytes(), b"unrelated conversion\n")
        self.assertTrue(self.destination.is_symlink())
        self.assertEqual(self.destination.readlink(), target)
        self.assert_refused(error)

    def test_existing_output_directory_is_preserved(self):
        self.destination.mkdir()
        previous = self.destination / "previous.webp"
        previous.write_bytes(b"previous conversion\n")
        error, _ = self.attempt_conversion()
        self.assertEqual(list(self.destination.iterdir()), [previous])
        self.assertEqual(previous.read_bytes(), b"previous conversion\n")
        self.assert_refused(error)

    def test_output_created_during_source_decode_is_not_overwritten(self):
        real_open = Image.open
        created = False

        def interleaved_open(path, *args, **kwargs):
            nonlocal created
            if path == self.source and not created:
                self.destination.write_bytes(b"another writer's conversion\n")
                created = True
            return real_open(path, *args, **kwargs)

        with patch.object(MOD.Image, "open", side_effect=interleaved_open):
            error, _ = self.attempt_conversion()
        self.assertTrue(created)
        self.assertEqual(self.destination.read_bytes(), b"another writer's conversion\n")
        self.assert_refused(error)

    def test_real_tiny_animation_preserves_pixels_timing_dimensions_and_loop(self):
        result = MOD.convert(self.source, self.destination)
        with Image.open(self.source) as gif, Image.open(self.destination) as webp:
            self.assertEqual(webp.format, "WEBP")
            self.assertEqual(webp.size, gif.size)
            self.assertEqual(webp.n_frames, 2)
            self.assertEqual(webp.info["loop"], 2)
            for index, duration in enumerate([70, 130]):
                gif.seek(index)
                webp.seek(index)
                self.assertEqual(webp.convert("RGB").tobytes(), gif.convert("RGB").tobytes())
                self.assertEqual(webp.info["duration"], duration)
        self.assertEqual(result["dimensions"], [8, 6])
        self.assertEqual(result["loop"], 2)
        self.assertEqual(result["duration_ms"], 200)
        self.assertTrue(result["lossless_rgb_and_timeline"])
        self.assertFalse(result["first_last_identical"])
        self.assertEqual(result["webp_bytes"], self.destination.stat().st_size)
        self.assertEqual(result["webp_sha256"], hashlib.sha256(self.destination.read_bytes()).hexdigest())
        self.assertEqual(self.source.read_bytes(), self.source_bytes)


if __name__ == "__main__":
    unittest.main()
