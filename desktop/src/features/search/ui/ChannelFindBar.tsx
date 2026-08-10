import { ChevronDown, ChevronUp, X } from "lucide-react";
import * as React from "react";

import type { useChannelFind } from "@/features/search/useChannelFind";
import { channelChrome } from "@/shared/layout/chromeLayout";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";

type ChannelFindBarProps = {
  /**
   * Bumped by every find-shortcut press. Re-focusing on change is what makes a
   * second ⌘F/Ctrl+F select the existing query instead of doing nothing.
   */
  focusRequestId?: number;
  matchCount: number;
  matchIndex: number;
  onClose: () => void;
  onNext: () => void;
  onPrevious: () => void;
  onQueryChange: (query: string) => void;
  query: string;
};

export function ChannelFindBar({
  focusRequestId = 0,
  matchCount,
  matchIndex,
  onClose,
  onNext,
  onPrevious,
  onQueryChange,
  query,
}: ChannelFindBarProps) {
  const inputRef = React.useRef<HTMLInputElement>(null);

  // biome-ignore lint/correctness/useExhaustiveDependencies: focusRequestId is the trigger, not an input — each press must re-focus and re-select.
  React.useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, [focusRequestId]);

  const handleKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }

    if (event.key === "Enter") {
      event.preventDefault();
      if (event.shiftKey) {
        onPrevious();
      } else {
        onNext();
      }
    }
  };

  const matchLabel =
    query.length >= 2
      ? matchCount > 0
        ? `${matchIndex + 1} of ${matchCount}`
        : "No results"
      : null;

  return (
    <div
      className="flex items-center gap-1.5 border-b border-border/80 bg-background px-3 py-1.5"
      data-testid="channel-find-bar"
    >
      <div className="relative flex min-w-0 flex-1 items-center">
        <input
          ref={inputRef}
          autoCapitalize="none"
          autoCorrect="off"
          className={cn(
            "h-7 w-full rounded-md border border-input bg-transparent px-2 pr-20 text-sm",
            "placeholder:text-muted-foreground",
            "focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring",
          )}
          onChange={(event) => onQueryChange(event.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Find in channel"
          spellCheck={false}
          type="text"
          value={query}
        />
        {matchLabel ? (
          <span className="pointer-events-none absolute right-2 text-xs text-muted-foreground">
            {matchLabel}
          </span>
        ) : null}
      </div>

      <Button
        aria-label="Previous match"
        className="h-7 w-7"
        disabled={matchCount === 0}
        onClick={onPrevious}
        size="icon"
        variant="ghost"
      >
        <ChevronUp className="h-4 w-4" />
      </Button>

      <Button
        aria-label="Next match"
        className="h-7 w-7"
        disabled={matchCount === 0}
        onClick={onNext}
        size="icon"
        variant="ghost"
      >
        <ChevronDown className="h-4 w-4" />
      </Button>

      <Button
        aria-label="Close find bar"
        className="h-7 w-7"
        onClick={onClose}
        size="icon"
        variant="ghost"
      >
        <X className="h-4 w-4" />
      </Button>
    </div>
  );
}

/**
 * The find bar in its slot above the message column, wired to the find state.
 *
 * Owning the mount decision here keeps "is the bar on screen" in one place —
 * the same place the shortcut's `canRenderFindBar` guard has to agree with.
 */
export function ChannelFindBarSlot({
  find,
}: {
  find: ReturnType<typeof useChannelFind>;
}) {
  if (!find.isOpen) {
    return null;
  }

  return (
    <div className={cn("absolute inset-x-0 z-40", channelChrome.top)}>
      <ChannelFindBar
        focusRequestId={find.focusRequestId}
        matchCount={find.matchCount}
        matchIndex={find.activeIndex}
        onClose={find.close}
        onNext={find.goToNext}
        onPrevious={find.goToPrevious}
        onQueryChange={find.setQuery}
        query={find.query}
      />
    </div>
  );
}
