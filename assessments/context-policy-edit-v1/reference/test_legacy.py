import unittest
from shipping import quote, tracking_label


class LegacyTests(unittest.TestCase):
    def test_prices(self):
        self.assertEqual(quote('local', 1000), 400)
        self.assertEqual(quote('remote', 1001), 1050)
        self.assertEqual(quote('local', 1, True), 1000)

    def test_label(self):
        self.assertEqual(tracking_label('ab-12'), 'TRACK:AB-12')
        with self.assertRaises(ValueError):
            tracking_label('é')


if __name__ == '__main__':
    unittest.main()
