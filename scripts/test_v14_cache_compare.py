import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("v14-cache-compare.py")
SPEC = importlib.util.spec_from_file_location("v14_cache_compare", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class CacheCompareTests(unittest.TestCase):
    def test_duration_units(self):
        self.assertEqual(MODULE.duration_us("8 us"), 8.0)
        self.assertEqual(MODULE.duration_us("1.5 ms"), 1500.0)

    def test_memory_units(self):
        self.assertEqual(MODULE.memory_mib("1024 KiB"), 1.0)
        self.assertEqual(MODULE.memory_mib("2 MiB"), 2.0)

    def test_percent_reduction(self):
        self.assertEqual(MODULE.percent_reduction(8.0, 6.0), 25.0)


if __name__ == "__main__":
    unittest.main()
