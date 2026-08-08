import assert from "node:assert/strict";
import test from "node:test";

import {
  findUnreachableSettingsSections,
  findUnregisteredNavSections,
  settingsNavGroups,
  settingsSections,
} from "./settingsNav.ts";

// BUG-024. `moderation` was registered in `settingsSections` (PR #1617) but
// never added to a nav group, so `SettingsView` -- which renders its sidebar
// from the groups, not the registry -- had no row that could open it. The panel
// shipped and no user could reach it. Nothing compared the two lists, so
// nothing noticed for months. This is that comparison.
test("every registered settings section is reachable from a nav group", () => {
  assert.deepEqual(
    findUnreachableSettingsSections(),
    [],
    "sections registered in settingsSections but listed in no nav group are " +
      "shipped UI that no user can navigate to; add each to a group in " +
      "settingsNavGroups, or drop the registration if it is dead code",
  );
});

// The mirror-image defect: SettingsView drops unknown nav entries silently, so
// a row left behind by a rename or deletion produces no error anywhere.
test("every nav group entry points at a registered settings section", () => {
  assert.deepEqual(
    findUnregisteredNavSections(),
    [],
    "nav-group entries with no matching settingsSections descriptor are " +
      "silently dropped by SettingsView",
  );
});

test("no settings section is listed by more than one nav group", () => {
  const seen = new Set();
  const duplicates = [];
  for (const group of settingsNavGroups) {
    for (const value of group.sections) {
      if (seen.has(value)) duplicates.push(value);
      seen.add(value);
    }
  }
  assert.deepEqual(duplicates, []);
});

// Named explicitly, because the generic check above would still pass if
// `moderation` were dropped from BOTH lists -- which is the shape the bug
// would take if someone "cleaned up" the unreachable entry instead of wiring
// it in. The queue is community-admin UI that is meant to exist.
test("the moderation queue is registered and reachable", () => {
  assert.ok(
    settingsSections.some((section) => section.value === "moderation"),
    "moderation must stay registered in settingsSections",
  );
  const owningGroups = settingsNavGroups
    .filter((group) => group.sections.includes("moderation"))
    .map((group) => group.label);
  assert.deepEqual(
    owningGroups,
    ["Communities"],
    "moderation belongs beside community-members, the other owner/admin " +
      "community surface",
  );
});

// Guards the helpers themselves: a reachability check that cannot report a
// miss is the same class of defect as the one it exists to catch.
test("findUnreachableSettingsSections reports a section missing from every group", () => {
  const sections = [
    { value: "profile", label: "Profile", icon: () => null },
    { value: "moderation", label: "Moderation", icon: () => null },
  ];
  const groups = [{ label: "Personal", sections: ["profile"] }];

  assert.deepEqual(findUnreachableSettingsSections(sections, groups), [
    "moderation",
  ]);
  assert.deepEqual(
    findUnreachableSettingsSections(sections, [
      { label: "Personal", sections: ["profile"] },
      { label: "Communities", sections: ["moderation"] },
    ]),
    [],
  );
});

test("findUnregisteredNavSections reports a nav entry with no descriptor", () => {
  const sections = [{ value: "profile", label: "Profile", icon: () => null }];
  const groups = [
    { label: "Personal", sections: ["profile"] },
    { label: "Communities", sections: ["relay-members"] },
  ];

  assert.deepEqual(findUnregisteredNavSections(sections, groups), [
    "Communities/relay-members",
  ]);
  assert.deepEqual(findUnregisteredNavSections(sections, [groups[0]]), []);
});
