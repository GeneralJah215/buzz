/**
 * Settings section registry + the navigation groups that expose it.
 *
 * These two lists are a matched pair: `settingsSections` says a section
 * exists and how to render its nav row, and `settingsNavGroups` says where in
 * the sidebar that row appears. A section registered in the first but absent
 * from the second is shipped code that no user can ever open -- which is
 * exactly what happened to `moderation` (see BUG-024): it was registered in
 * #1617 without a matching nav-group entry, and stayed invisible for months
 * because nothing compared the two lists.
 *
 * They live in the same module, free of JSX, so the pairing can be asserted by
 * a unit test rather than trusted. `findUnreachableSettingsSections` and
 * `findUnregisteredNavSections` are that assertion's two directions.
 */
import {
  Archive,
  BellRing,
  Bot,
  Cpu,
  Download,
  FlaskConical,
  Keyboard,
  LayoutTemplate,
  MessagesSquare,
  MonitorCog,
  ShieldAlert,
  Smartphone,
  Smile,
  Ticket,
  UserRound,
  Waypoints,
  Volume2,
  type LucideIcon,
} from "lucide-react";

export type SettingsSection =
  | "profile"
  | "notifications"
  | "voice"
  | "experimental"
  | "agents"
  | "channel-templates"
  | "compute"
  | "appearance"
  | "shortcuts"
  | "hosted-communities"
  | "community-members"
  | "moderation"
  | "custom-emoji"
  | "local-archive"
  | "edge-sync"
  | "mobile"
  | "updates";

export const DEFAULT_SETTINGS_SECTION: SettingsSection = "profile";

const SETTINGS_SECTION_VALUES: readonly SettingsSection[] = [
  "profile",
  "notifications",
  "voice",
  "experimental",
  "agents",
  "channel-templates",
  "compute",
  "appearance",
  "shortcuts",
  "hosted-communities",
  "community-members",
  "moderation",
  "custom-emoji",
  "local-archive",
  "edge-sync",
  "mobile",
  "updates",
];

export function isSettingsSection(value: unknown): value is SettingsSection {
  return (
    typeof value === "string" &&
    (SETTINGS_SECTION_VALUES as readonly string[]).includes(value)
  );
}

export type SettingsSectionDescriptor = {
  value: SettingsSection;
  label: string;
  icon: LucideIcon;
  /** If set, this section is only visible when the feature is enabled */
  featureGate?: string;
};

export const settingsSections: SettingsSectionDescriptor[] = [
  {
    value: "appearance",
    label: "Appearance",
    icon: MonitorCog,
  },
  {
    value: "profile",
    label: "Profile",
    icon: UserRound,
  },
  {
    value: "notifications",
    label: "Notifications",
    icon: BellRing,
  },
  {
    value: "voice",
    label: "Voice",
    icon: Volume2,
  },
  {
    value: "experimental",
    label: "Experiments",
    icon: FlaskConical,
  },
  {
    value: "agents",
    label: "Agents",
    icon: Bot,
    featureGate: "managed-agents",
  },
  {
    value: "channel-templates",
    label: "Channel templates",
    icon: LayoutTemplate,
    featureGate: "channel-templates",
  },
  {
    value: "compute",
    label: "Compute",
    icon: Cpu,
  },
  {
    value: "shortcuts",
    label: "Shortcuts",
    icon: Keyboard,
  },
  {
    value: "hosted-communities",
    label: "Hosted communities",
    icon: MessagesSquare,
  },
  {
    value: "community-members",
    label: "Invites",
    icon: Ticket,
  },
  {
    value: "moderation",
    label: "Moderation",
    icon: ShieldAlert,
  },
  {
    value: "custom-emoji",
    label: "Custom emoji",
    icon: Smile,
    featureGate: "custom-emoji",
  },
  {
    value: "local-archive",
    label: "Local archive",
    icon: Archive,
  },
  {
    // Hidden unless a local edge sidecar actually answers. See
    // `isEdgeSyncSectionVisible` and the runtime filter in `SettingsView`:
    // the feature is off by default, and a user who has never heard of it
    // must not find a nav entry advertising it.
    value: "edge-sync",
    label: "Local sync",
    icon: Waypoints,
  },
  {
    value: "mobile",
    label: "Mobile",
    icon: Smartphone,
  },
  {
    value: "updates",
    label: "Updates",
    icon: Download,
  },
];

export type SettingsNavGroup = {
  label: string;
  sections: SettingsSection[];
};

export const settingsNavGroups: SettingsNavGroup[] = [
  {
    label: "Personal",
    sections: [
      "profile",
      "appearance",
      "notifications",
      "voice",
      "shortcuts",
      "custom-emoji",
      "local-archive",
      "channel-templates",
    ],
  },
  {
    label: "Communities",
    sections: [
      "hosted-communities",
      "community-members",
      // Owner/admin only, like `community-members` beside it. `SettingsView`
      // applies that runtime gate; `ModerationQueueCard` applies the same one
      // itself because the relay 403s a member who asks for /moderation/*.
      "moderation",
    ],
  },
  {
    label: "App",
    sections: [
      "agents",
      "compute",
      "experimental",
      "edge-sync",
      "mobile",
      "updates",
    ],
  },
];

/**
 * Registered sections that no nav group lists, in registration order.
 *
 * A non-empty result means shipped, reachable-by-code settings UI that no user
 * can navigate to. Runtime visibility gates (`featureGate`, the membership and
 * sidecar checks in `SettingsView`) are deliberately ignored here: a gated
 * section is still *addressable* -- some user, in some state, can open it --
 * whereas a section missing from every group is addressable by nobody.
 */
export function findUnreachableSettingsSections(
  sections: readonly SettingsSectionDescriptor[] = settingsSections,
  groups: readonly SettingsNavGroup[] = settingsNavGroups,
): SettingsSection[] {
  const grouped = new Set<string>(groups.flatMap((group) => group.sections));
  return sections
    .map((section) => section.value)
    .filter((value) => !grouped.has(value));
}

/**
 * Nav-group entries with no matching registered section, as
 * `"<group label>/<section>"` pairs.
 *
 * The mirror-image defect: a nav row pointing at a section that was renamed or
 * deleted. `SettingsView` filters those out silently, so without this check a
 * stale entry leaves no trace at all.
 */
export function findUnregisteredNavSections(
  sections: readonly SettingsSectionDescriptor[] = settingsSections,
  groups: readonly SettingsNavGroup[] = settingsNavGroups,
): string[] {
  const registered = new Set<string>(sections.map((section) => section.value));
  return groups.flatMap((group) =>
    group.sections
      .filter((value) => !registered.has(value))
      .map((value) => `${group.label}/${value}`),
  );
}
