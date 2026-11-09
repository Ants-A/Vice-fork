import json
import tempfile
import unittest
from pathlib import Path

try:
    import _sandbox  # noqa: F401  keeps every store out of the real home
except ModuleNotFoundError:
    from tests import _sandbox  # noqa: F401

from vice.playlists import (PlaylistStore, build_tag_index,
                            decode_playlist_metadata, encode_playlist_metadata,
                            game_key)


class PlaylistStoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.path = Path(self.tmpdir.name) / "playlists.json"

    def test_missing_and_corrupt_files_load_as_empty(self) -> None:
        store = PlaylistStore(self.path)
        self.assertEqual(store.list_playlists(), [])

        self.path.write_text("{not json")
        store = PlaylistStore(self.path)
        self.assertEqual(store.list_playlists(), [])

    def test_game_key_matches_recorder_filename_sanitizing(self) -> None:
        self.assertEqual(game_key("Overwatch 2"), "overwatch-2")
        self.assertEqual(game_key("Overwatch-2"), "overwatch-2")
        self.assertEqual(game_key("Deep Rock Galactic"), "deep-rock-galactic")

    def test_record_auto_creates_once_and_dedups(self) -> None:
        store = PlaylistStore(self.path)
        self.assertTrue(store.record_auto("Overwatch 2", "Vice_Clip_1"))
        self.assertFalse(store.record_auto("Overwatch 2", "Vice_Clip_1"))
        self.assertTrue(store.record_auto("Overwatch 2", "Vice_Clip_2"))

        playlists = store.list_playlists()
        self.assertEqual(len(playlists), 1)
        self.assertEqual(playlists[0]["id"], "auto:overwatch-2")
        self.assertEqual(playlists[0]["clip_slugs"], ["Vice_Clip_1", "Vice_Clip_2"])

    def test_auto_playlist_colors_are_deterministic(self) -> None:
        first = PlaylistStore(self.path)
        first.record_auto("Minecraft", "a")
        colors = (first.get("auto:minecraft")["color1"], first.get("auto:minecraft")["color2"])

        other = PlaylistStore(Path(self.tmpdir.name) / "other.json")
        other.record_auto("Minecraft", "b")
        self.assertEqual(
            colors,
            (other.get("auto:minecraft")["color1"], other.get("auto:minecraft")["color2"]),
        )

    def test_custom_crud_validation(self) -> None:
        store = PlaylistStore(self.path)
        with self.assertRaises(ValueError):
            store.create_custom("   ")
        with self.assertRaises(ValueError):
            store.create_custom("Fails", emoji="toolong")

        p = store.create_custom("Fails", emoji="😂", color1="#38bdf8", color2="#075985")
        self.assertEqual(p["kind"], "custom")
        self.assertEqual(p["color1"], "#38bdf8")

        bad = store.create_custom("NoColors", color1="red", color2="javascript:x")
        self.assertRegex(bad["color1"], r"^#[0-9a-fA-F]{6}$")
        self.assertRegex(bad["color2"], r"^#[0-9a-fA-F]{6}$")

        store.update_playlist(p["id"], {"name": "Epic fails"})
        self.assertEqual(store.get(p["id"])["name"], "Epic fails")
        with self.assertRaises(KeyError):
            store.update_playlist("missing", {"name": "X"})

        store.delete(p["id"])
        self.assertIsNone(store.get(p["id"]))

    def test_auto_playlists_can_be_edited_and_stay_auto(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")

        edited = store.update_playlist("auto:minecraft", {"name": "MC", "emoji": "⛏️", "color1": "#123456"})
        self.assertEqual(edited["name"], "MC")
        self.assertEqual(edited["emoji"], "⛏️")
        self.assertEqual(edited["kind"], "auto")  # still auto: new clips keep filing in
        self.assertTrue(edited["edited"])

        # A new clip of the same game still lands in the edited playlist and
        # does not overwrite the user's name.
        store.record_auto("Minecraft", "clip2")
        self.assertEqual(store.get("auto:minecraft")["name"], "MC")
        self.assertIn("clip2", store.get("auto:minecraft")["clip_slugs"])

    def test_deleting_an_auto_playlist_sticks_across_restart(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "Vice_Clip_1_Minecraft")
        store.delete("auto:minecraft")
        self.assertIsNone(store.get("auto:minecraft"))

        # Backfill from the still-tagged clip must not resurrect it.
        reloaded = PlaylistStore(self.path)
        reloaded.backfill({"Vice_Clip_1_Minecraft"}, {})
        self.assertIsNone(reloaded.get("auto:minecraft"))

        # But clipping the game again revives it.
        reloaded.record_auto("Minecraft", "Vice_Clip_2_Minecraft")
        self.assertIsNotNone(reloaded.get("auto:minecraft"))

    def test_edited_auto_playlist_is_not_pruned_when_empty(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")
        store.update_playlist("auto:minecraft", {"emoji": "⛏️"})

        store.remove_clip("auto:minecraft", "clip")
        self.assertIsNotNone(store.get("auto:minecraft"))

    def test_rename_sweeps_every_playlist(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "old")
        custom = store.create_custom("Best")
        store.add_clip(custom["id"], "old")

        self.assertTrue(store.on_clip_renamed("old", "new"))
        self.assertEqual(store.get("auto:minecraft")["clip_slugs"], ["new"])
        self.assertEqual(store.get(custom["id"])["clip_slugs"], ["new"])
        self.assertFalse(store.on_clip_renamed("gone", "elsewhere"))

    def test_delete_prunes_and_drops_empty_auto_playlists(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")
        custom = store.create_custom("Best")
        store.add_clip(custom["id"], "clip")

        self.assertTrue(store.on_clip_deleted("clip"))
        self.assertIsNone(store.get("auto:minecraft"))
        self.assertEqual(store.get(custom["id"])["clip_slugs"], [])

    def test_remove_clip_drops_emptied_auto_playlist(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")
        store.remove_clip("auto:minecraft", "clip")
        self.assertIsNone(store.get("auto:minecraft"))

    def test_game_for_reports_auto_membership_only(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Overwatch 2", "clip")
        custom = store.create_custom("Best")
        store.add_clip(custom["id"], "other")
        self.assertEqual(store.game_for("clip"), "Overwatch 2")
        self.assertIsNone(store.game_for("other"))

    def test_backfill_seeds_tags_and_prunes_missing_files(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "Vice_Clip_9_Minecraft")
        custom = store.create_custom("Best")
        store.add_clip(custom["id"], "Vice_Clip_9_Minecraft")

        on_disk = {"Vice_Clip_1_Overwatch-2", "Vice_Clip_2", "MyRenamedClip"}
        index = {"overwatch-2": "Overwatch 2"}
        self.assertTrue(store.backfill(on_disk, index))

        ids = [p["id"] for p in store.list_playlists()]
        self.assertIn("auto:overwatch-2", ids)
        self.assertNotIn("auto:minecraft", ids)
        self.assertEqual(store.get("auto:overwatch-2")["name"], "Overwatch 2")
        self.assertEqual(store.get("auto:overwatch-2")["clip_slugs"], ["Vice_Clip_1_Overwatch-2"])
        self.assertEqual(store.get(custom["id"])["clip_slugs"], [])

    def test_backfill_uses_readable_fallback_for_unknown_tags(self) -> None:
        store = PlaylistStore(self.path)
        store.backfill({"Vice_Clip_1_Some-Indie-Game"}, {})
        auto = [p for p in store.list_playlists() if p["kind"] == "auto"][0]
        self.assertEqual(auto["name"], "Some Indie Game")

    def test_state_survives_reload(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")
        custom = store.create_custom("Best", emoji="🔥")
        store.add_clip(custom["id"], "clip")

        reloaded = PlaylistStore(self.path)
        self.assertEqual(len(reloaded.list_playlists()), 2)
        self.assertEqual(reloaded.get(custom["id"])["emoji"], "🔥")
        self.assertEqual(reloaded.game_for("clip"), "Minecraft")

    def test_saved_file_is_valid_json_with_version(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip")
        data = json.loads(self.path.read_text())
        self.assertEqual(data["version"], 1)
        self.assertEqual(len(data["playlists"]), 1)

    def test_load_reports_whether_the_file_was_usable(self) -> None:
        missing = PlaylistStore(self.path)
        self.assertFalse(missing.loaded_from_disk)
        self.assertEqual(missing.load_error, "")

        self.path.write_text("{not json")
        corrupt = PlaylistStore(self.path)
        self.assertFalse(corrupt.loaded_from_disk)
        self.assertTrue(corrupt.load_error)

        PlaylistStore(self.path).create_custom("Usable")
        usable = PlaylistStore(self.path)
        self.assertTrue(usable.loaded_from_disk)
        self.assertEqual(usable.load_error, "")

    def test_playlist_metadata_round_trips_every_definition(self) -> None:
        store = PlaylistStore(self.path)
        custom = store.create_custom(
            "Fails", emoji="🔥", color1="#112233", color2="#445566",
        )
        store.add_clip(custom["id"], "clip_a")
        store.record_auto("Minecraft", "clip_a")
        store.update_playlist("auto:minecraft", {"name": "MC"})

        raw = store.metadata_for("clip_a")
        records = {r["id"]: r for r in decode_playlist_metadata(raw)}
        self.assertEqual(set(records), {custom["id"], "auto:minecraft"})

        saved = records[custom["id"]]
        self.assertEqual(saved["name"], "Fails")
        self.assertEqual(saved["emoji"], "🔥")
        self.assertEqual(saved["color1"], "#112233")
        self.assertEqual(saved["color2"], "#445566")
        self.assertNotIn("clip_slugs", saved)

        auto = records["auto:minecraft"]
        self.assertEqual(auto["kind"], "auto")
        self.assertEqual(auto["name"], "MC")
        self.assertEqual(auto["game"], "Minecraft")
        self.assertTrue(auto["edited"])

        # A video outside the playlist gets an empty definition list.
        self.assertEqual(
            decode_playlist_metadata(store.metadata_for("other_clip")), [],
        )

    def test_metadata_decoding_ignores_untrusted_fields(self) -> None:
        raw = json.dumps({"v": 1, "playlists": [
            "junk",
            {},
            {"id": ""},
            {
                "id": "p1",
                "kind": "not-a-kind",
                "name": "",
                "emoji": "abcdefgh",
                "color1": "javascript:x",
                "color2": "#ABCDEF",
                "rev": "not-a-number",
            },
        ]})

        records = decode_playlist_metadata(raw)
        self.assertEqual(len(records), 1)
        record = records[0]
        self.assertEqual(record["id"], "p1")
        self.assertEqual(record["kind"], "custom")
        self.assertEqual(record["name"], "p1")
        self.assertEqual(record["emoji"], "abcd")
        self.assertEqual(record["color1"], "")
        self.assertEqual(record["color2"], "#ABCDEF")
        self.assertEqual(record["rev"], 0)

        for bad in ("not json", None, [], {"playlists": "no"}, {"playlists": [1]}):
            self.assertEqual(decode_playlist_metadata(bad), [])

    def test_restore_from_metadata_rebuilds_definitions_and_membership(self) -> None:
        source = PlaylistStore(Path(self.tmpdir.name) / "source.json")
        custom = source.create_custom(
            "Best", emoji="🏆", color1="#123456", color2="#654321",
        )
        for slug in ("clip_a", "clip_b"):
            source.add_clip(custom["id"], slug)
            source.record_auto("Minecraft", slug)
        entries = [(slug, source.metadata_for(slug))
                   for slug in ("clip_a", "clip_b")]

        restored = PlaylistStore(self.path)
        self.assertFalse(restored.loaded_from_disk)
        self.assertTrue(restored.restore_from_metadata(entries))

        rebuilt = restored.get(custom["id"])
        self.assertEqual(rebuilt["name"], "Best")
        self.assertEqual(rebuilt["emoji"], "🏆")
        self.assertEqual(rebuilt["color1"], "#123456")
        self.assertEqual(sorted(rebuilt["clip_slugs"]), ["clip_a", "clip_b"])
        self.assertEqual(
            sorted(restored.get("auto:minecraft")["clip_slugs"]),
            ["clip_a", "clip_b"],
        )

        # The rebuild is written back, so it survives even if the tags are
        # removed from the videos afterwards.
        reloaded = PlaylistStore(self.path)
        self.assertTrue(reloaded.loaded_from_disk)
        self.assertEqual(reloaded.get(custom["id"])["name"], "Best")
        self.assertEqual(
            sorted(reloaded.get(custom["id"])["clip_slugs"]),
            ["clip_a", "clip_b"],
        )

    def test_restore_prefers_the_newest_definition_but_unions_members(self) -> None:
        def playlist(name: str, slug: str) -> dict:
            return {
                "id": "pl-same",
                "kind": "custom",
                "name": name,
                "clip_slugs": [slug],
            }

        entries = [
            ("clip_a", encode_playlist_metadata([playlist("Old", "clip_a")],
                                               "clip_a", rev=10)),
            ("clip_b", encode_playlist_metadata([playlist("New", "clip_b")],
                                               "clip_b", rev=20)),
        ]
        restored = PlaylistStore(self.path)
        self.assertTrue(restored.restore_from_metadata(entries))
        self.assertEqual(restored.get("pl-same")["name"], "New")
        self.assertEqual(restored.get("pl-same")["clip_slugs"], ["clip_a", "clip_b"])

    def test_merge_metadata_does_not_overwrite_the_json_definitions(self) -> None:
        store = PlaylistStore(self.path)
        custom = store.create_custom("Current")
        store.add_clip(custom["id"], "clip_a")

        stale = dict(custom, name="Stale", emoji="🕰️")
        stale["clip_slugs"] = ["clip_a", "clip_b"]
        extra = {
            "id": "pl-tagged",
            "kind": "custom",
            "name": "From tag",
            "emoji": "📼",
            "color1": "#111111",
            "color2": "#222222",
            "clip_slugs": ["clip_b"],
        }
        entries = [("clip_b", encode_playlist_metadata([stale, extra], "clip_b"))]

        self.assertTrue(store.merge_metadata(entries))
        self.assertEqual(store.get(custom["id"])["name"], "Current")
        self.assertEqual(store.get(custom["id"])["emoji"], "")
        self.assertEqual(
            sorted(store.get(custom["id"])["clip_slugs"]),
            ["clip_a", "clip_b"],
        )
        self.assertEqual(store.get("pl-tagged")["name"], "From tag")
        self.assertEqual(store.get("pl-tagged")["emoji"], "📼")
        self.assertEqual(store.get("pl-tagged")["clip_slugs"], ["clip_b"])

        # Merging the same tags again changes nothing.
        self.assertFalse(store.merge_metadata(entries))
        self.assertIn("pl-tagged", [p["id"] for p in PlaylistStore(self.path).list_playlists()])

    def test_merge_metadata_does_not_revive_a_deleted_auto_playlist(self) -> None:
        store = PlaylistStore(self.path)
        store.record_auto("Minecraft", "clip_a")
        store.delete("auto:minecraft")

        tagged = {
            "id": "auto:minecraft",
            "kind": "auto",
            "name": "Minecraft",
            "game": "Minecraft",
            "clip_slugs": ["clip_b"],
        }
        entries = [("clip_b", encode_playlist_metadata([tagged], "clip_b"))]
        self.assertFalse(store.merge_metadata(entries))
        self.assertIsNone(store.get("auto:minecraft"))


class TagIndexTests(unittest.TestCase):
    def test_bundled_games_are_indexed_by_sanitized_key(self) -> None:
        index = build_tag_index()
        self.assertEqual(index.get("minecraft"), "Minecraft")

    def test_custom_games_extend_the_index(self) -> None:
        class _Game:
            name = "My Custom Game"

        index = build_tag_index([_Game()])
        self.assertEqual(index.get("my-custom-game"), "My Custom Game")


if __name__ == "__main__":
    unittest.main()
