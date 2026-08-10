import type * as React from "react";

import { THREAD_FOCUS_COLUMN_MAX_WIDTH_PX } from "@/features/channels/lib/threadFocusLayout";
import { AUXILIARY_PANEL_SINGLE_COLUMN_BREAKPOINT_PX } from "@/shared/layout/AuxiliaryPanel";
import type { ChannelType } from "@/shared/api/types";

/**
 * Whether an auxiliary panel is open, and whether it has taken over the whole
 * channel pane.
 *
 * `isSinglePanelView` means the MAIN COLUMN IS NOT RENDERED — timeline,
 * composer and the find bar all disappear with it. Anything that assumes those
 * surfaces exist (the ⌘F/Ctrl+F shortcut most of all, BUG-059) has to consult
 * this, which is why it is a named selector rather than an inline expression
 * buried halfway down ChannelScreen.
 */
export function selectChannelPanelLayout({
  channelContentWidthPx,
  channelManagementOpen,
  channelType,
  openAgentSessionPubkey,
  openThreadHeadId,
  profilePanelPubkey,
}: {
  channelContentWidthPx: number;
  channelManagementOpen: boolean;
  channelType: ChannelType | null;
  openAgentSessionPubkey: string | null;
  openThreadHeadId: string | null;
  profilePanelPubkey: string | null;
}): { hasAuxiliaryPanel: boolean; isSinglePanelView: boolean } {
  const hasAuxiliaryPanel = Boolean(
    openThreadHeadId ||
      openAgentSessionPubkey ||
      profilePanelPubkey ||
      channelManagementOpen,
  );
  const isNarrowPanelViewport =
    channelContentWidthPx > 0 &&
    channelContentWidthPx < AUXILIARY_PANEL_SINGLE_COLUMN_BREAKPOINT_PX;

  return {
    hasAuxiliaryPanel,
    isSinglePanelView:
      isNarrowPanelViewport && channelType !== "forum" && hasAuxiliaryPanel,
  };
}

export type ThreadPanelLayoutProps = {
  columnMaxWidthPx?: number;
  headerLeading?: React.ReactNode;
  isFocusMode: boolean;
  isSinglePanelView?: boolean;
  layout?: "standalone" | "split";
  transparentChrome?: boolean;
};

type ThreadPanelLayoutOptions = {
  headerLeading?: React.ReactNode;
  isFocusDrawer: boolean;
  isSinglePanelView: boolean;
  useSplitAuxiliaryPane: boolean;
};

/** Maps channel presentation into the shared thread-panel layout contract. */
export function getThreadPanelLayout({
  headerLeading,
  isFocusDrawer,
  isSinglePanelView,
  useSplitAuxiliaryPane,
}: ThreadPanelLayoutOptions): ThreadPanelLayoutProps {
  return isFocusDrawer
    ? {
        columnMaxWidthPx: THREAD_FOCUS_COLUMN_MAX_WIDTH_PX,
        headerLeading,
        isFocusMode: true,
        isSinglePanelView: true,
        layout: "standalone",
        transparentChrome: false,
      }
    : {
        columnMaxWidthPx: undefined,
        headerLeading,
        isFocusMode: false,
        isSinglePanelView: useSplitAuxiliaryPane ? false : isSinglePanelView,
        layout: useSplitAuxiliaryPane ? "split" : "standalone",
        transparentChrome: useSplitAuxiliaryPane,
      };
}
