import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    import _sandbox  # noqa: F401  keeps every store out of the real home
except ModuleNotFoundError:
    from tests import _sandbox  # noqa: F401

from vice import game_icons
from vice.config import Config, DiscordConfig
from vice.main import ViceDaemon

APPS = [
    {"id": "1", "name": "THE FINALS"},
    {"id": "2", "name": "The Finals"},
    {"id": "", "name": "No Id"},
    {"id": "3", "name": "  Deltarune "},
]


class _CacheTestCase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        patcher = mock.patch("vice.game_icons.cache_dir", return_value=self.dir)
        patcher.start()
        self.addCleanup(patcher.stop)

    def icons(self) -> dict:
        return json.loads((self.dir / "icons.json").read_text())


class NameMapTests(unittest.TestCase):
    def test_names_are_lowercased_and_the_first_entry_wins(self) -> None:
        names = game_icons.build_name_map(APPS)

        self.assertEqual(names["the finals"], "1")
        self.assertEqual(names["deltarune"], "3")

    def test_entries_without_an_id_or_name_are_skipped(self) -> None:
        names = game_icons.build_name_map(APPS + [{"id": "9", "name": ""}])

        self.assertNotIn("no id", names)
        self.assertNotIn("", names)

    def test_a_missing_list_gives_an_empty_map(self) -> None:
        self.assertEqual(game_icons.build_name_map(None), {})


class ResolveTests(_CacheTestCase):
    def test_a_found_icon_is_cached_as_a_cdn_url(self) -> None:
        with mock.patch("vice.game_icons._fetch_json", side_effect=[APPS, {"icon": "abc"}]):
            game_icons._resolve("the finals")

        self.assertEqual(
            self.icons()["the finals"],
            "https://cdn.discordapp.com/app-icons/1/abc.png",
        )

    def test_a_game_discord_does_not_know_is_cached_as_a_miss(self) -> None:
        with mock.patch("vice.game_icons._fetch_json", return_value=APPS):
            game_icons._resolve("unknown game")

        self.assertEqual(self.icons()["unknown game"], "")

    def test_a_failed_icon_lookup_is_not_cached(self) -> None:
        with mock.patch("vice.game_icons._fetch_json", side_effect=[APPS, OSError("boom")]):
            game_icons._resolve("the finals")

        self.assertFalse((self.dir / "icons.json").exists())

    def test_an_unavailable_game_list_is_not_cached(self) -> None:
        with mock.patch("vice.game_icons._fetch_json", side_effect=OSError("offline")):
            game_icons._resolve("the finals")

        self.assertFalse((self.dir / "icons.json").exists())


class IconUrlTests(_CacheTestCase):
    def setUp(self) -> None:
        super().setUp()
        game_icons._inflight.clear()

    def test_a_cache_hit_returns_the_url_without_a_lookup(self) -> None:
        (self.dir / "icons.json").write_text(json.dumps({"deltarune": "https://x/y.png"}))

        with mock.patch("vice.game_icons.threading.Thread") as thread:
            url = game_icons.discord_icon_url("  Deltarune ")

        self.assertEqual(url, "https://x/y.png")
        thread.assert_not_called()

    def test_a_cached_miss_returns_none_without_a_lookup(self) -> None:
        (self.dir / "icons.json").write_text(json.dumps({"unknown": ""}))

        with mock.patch("vice.game_icons.threading.Thread") as thread:
            self.assertIsNone(game_icons.discord_icon_url("Unknown"))

        thread.assert_not_called()

    def test_a_cache_miss_starts_one_lookup_and_returns_none(self) -> None:
        with mock.patch("vice.game_icons.threading.Thread") as thread:
            self.assertIsNone(game_icons.discord_icon_url("The Finals"))
            self.assertIsNone(game_icons.discord_icon_url("The Finals"))

        thread.assert_called_once()

    def test_a_blank_name_does_nothing(self) -> None:
        with mock.patch("vice.game_icons.threading.Thread") as thread:
            self.assertIsNone(game_icons.discord_icon_url(""))

        thread.assert_not_called()


class ActivityPayloadTests(unittest.TestCase):
    def _daemon(self, game_icons_on: bool) -> ViceDaemon:
        daemon = ViceDaemon.__new__(ViceDaemon)
        daemon.cfg = Config(discord=DiscordConfig(game_icons=game_icons_on))
        daemon._discord_started_at = 1000.0
        return daemon

    def test_a_found_icon_is_the_large_image_with_the_vice_badge(self) -> None:
        with mock.patch("vice.game_icons.discord_icon_url", return_value="https://x/y.png"):
            assets = self._daemon(True)._discord_activity("Deltarune")["assets"]

        self.assertEqual(assets["large_image"], "https://x/y.png")
        self.assertEqual(assets["small_image"], "vice_logo")

    def test_no_icon_sends_exactly_the_plain_vice_logo(self) -> None:
        with mock.patch("vice.game_icons.discord_icon_url", return_value=None):
            assets = self._daemon(True)._discord_activity("Deltarune")["assets"]

        self.assertEqual(
            assets,
            {"large_image": "vice_logo", "large_text": "Vice, Linux clip recorder"},
        )

    def test_the_setting_off_sends_the_plain_logo_and_never_looks_up(self) -> None:
        with mock.patch("vice.game_icons.discord_icon_url") as lookup:
            assets = self._daemon(False)._discord_activity("Deltarune")["assets"]

        lookup.assert_not_called()
        self.assertEqual(
            assets,
            {"large_image": "vice_logo", "large_text": "Vice, Linux clip recorder"},
        )


if __name__ == "__main__":
    unittest.main()
