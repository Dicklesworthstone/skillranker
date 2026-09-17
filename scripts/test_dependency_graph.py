"""Adversarial checks for the feature guard; the real Cargo graph is a separate gate."""
import unittest

from check_dependency_graph import audit_tree, check_manifest, feature_sets


GOOD = """skillranker v0.1.0 (/checkout)|default
asupersync v0.5.0 (https://example.invalid/runtime#abcdef)|native-runtime,runtime-core
frankensearch-core v0.3.1 (https://example.invalid/search#abcdef)|
frankensearch-quill v0.3.1 (https://example.invalid/search#abcdef)|
asupersync v0.5.0 (https://example.invalid/runtime#abcdef)|native-runtime,runtime-core (*)
"""


class DependencyGraphTests(unittest.TestCase):
    def test_shipping_graph_and_duplicate_display_rows_are_accepted(self):
        report = audit_tree(GOOD)
        self.assertEqual(report["package_count"], 4)
        self.assertEqual(report["required_features"]["frankensearch-quill"], [])

    def test_prohibited_packages_on_any_project_edge_are_refused(self):
        for name in ("tantivy", "tantivy-common", "tokio", "reqwest", "ureq", "fastembed",
                     "candle-core", "ort", "torch-sys", "meta_skill", "frankensearch",
                     "frankensearch-lexical", "frankensearch-embed", "frankensearch-quill-gauntlet",
                     "frankensearch-fusion", "frankensearch-models"):
            with self.subTest(package=name), self.assertRaisesRegex(ValueError, "prohibited dependency"):
                audit_tree(GOOD + f"{name} v1.0.0|\n")

    def test_optional_oracle_and_internal_features_cannot_hide_behind_safe_packages(self):
        for name, feature in (("ordinary", "cass-compat"), ("ordinary", "lexical-tantivy"),
                              ("ordinary", "tantivy-oracle"), ("frankensearch-quill", "durability"),
                              ("frankensearch-quill", "bench-internals"),
                              ("frankensearch-quill", "conformance-internals"),
                              ("frankensearch-quill", "profile-internals"),
                              ("frankensearch-quill", "pruning-conformance"),
                              ("frankensearch-quill", "default"),
                              ("frankensearch-core", "bench-internals")):
            suffix = f"{name} v1.0.0|{feature}\n"
            if name.startswith("frankensearch-"):
                line = next(line for line in GOOD.splitlines() if line.startswith(name + " "))
                output = GOOD.replace(line, line + feature)
            else:
                output = GOOD + suffix
            with self.subTest(package=name, feature=feature), self.assertRaisesRegex(ValueError, "prohibited features"):
                audit_tree(output)

    def test_missing_core_or_split_runtime_source_fails(self):
        for name in ("asupersync", "frankensearch-core", "frankensearch-quill"):
            with self.subTest(missing=name), self.assertRaisesRegex(ValueError, "exactly one"):
                audit_tree("\n".join(line for line in GOOD.splitlines() if not line.startswith(name + " ")))
            with self.subTest(duplicate=name), self.assertRaisesRegex(ValueError, "exactly one"):
                audit_tree(GOOD + f"{name} v0.5.0 (https://example.invalid/other#123456)|\n")

    def test_malformed_output_is_not_a_successful_empty_graph(self):
        for output in ("", "not cargo output", GOOD + "unrecognized row\n"):
            with self.subTest(output=output), self.assertRaises(ValueError):
                audit_tree(output)

    def test_all_combinations_include_default_disabled_and_enabled(self):
        # `b` can be an implicit optional-dependency feature returned by Cargo.
        matrix = feature_sets({"default": ["a"], "a": [], "b": ["dep:b"]})
        self.assertEqual(len(matrix), 8)
        self.assertEqual(len({tuple(flags) for _, flags in matrix}), 8)
        self.assertIn(("no-default:a,b", ["--no-default-features", "--features", "a,b"]), matrix)
        self.assertIn(("default:", []), matrix)
        with self.assertRaisesRegex(ValueError, "feature matrix"):
            feature_sets({str(index): [] for index in range(9)})

    def test_manifest_requires_narrow_explicit_dependencies(self):
        manifest = {"dependencies": {name: {"default-features": False,
                                           "git": "https://example.invalid/search", "rev": "a" * 40} for name in
                                    ("frankensearch-core", "frankensearch-quill")}}
        check_manifest(manifest)
        for value in ("0.3", {}, {"default-features": True},
                      {"default-features": False, "features": ["bench-internals"]}):
            with self.subTest(value=value), self.assertRaises(ValueError):
                check_manifest({"dependencies": {**manifest["dependencies"], "frankensearch-quill": value}})

    def test_sibling_paths_mutable_revisions_and_mismatched_sources_fail(self):
        source = {"default-features": False, "git": "https://example.invalid/search", "rev": "a" * 40}
        for override in ({"path": "../search"}, {"rev": "main"}, {"rev": "b" * 40},
                         {"git": "file:///sibling/search"}, {"git": "https://example.invalid/other"}):
            with self.subTest(override=override), self.assertRaises(ValueError):
                check_manifest({"dependencies": {"frankensearch-core": source,
                                                  "frankensearch-quill": {**source, **override}}})


if __name__ == "__main__":
    unittest.main()
