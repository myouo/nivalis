"use client";

import {
  FormEvent,
  KeyboardEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  type CdCompletionCandidate,
  completeInput,
  displayPath,
  getCdCompletionCandidates,
  type OutputLine,
  runCommand,
  type ThemeName,
} from "@/lib/shell";

type TerminalEntry =
  | {
      id: number;
      type: "command";
      command: string;
      cwd: string;
    }
  | {
      id: number;
      type: "output";
      line: OutputLine;
    };

type HitsResponse = {
  hits: number;
};

type CdCompletionSession = {
  candidates: CdCompletionCandidate[];
  index: number;
};

const COMPLETION_LIST_ID = "terminal-cd-completions";

let visitRequest: Promise<number> | null = null;

async function requestHits(method: "GET" | "POST"): Promise<number> {
  const response = await fetch("/api/hits", {
    method,
    cache: "no-store",
    headers: {
      Accept: "application/json",
    },
  });

  if (!response.ok) {
    throw new Error(`Visit counter returned ${response.status}`);
  }

  const payload = (await response.json()) as Partial<HitsResponse>;
  if (
    typeof payload.hits !== "number" ||
    !Number.isFinite(payload.hits) ||
    payload.hits < 0
  ) {
    throw new Error("Visit counter returned an invalid value");
  }

  return payload.hits;
}

function recordVisit(): Promise<number> {
  visitRequest ??= requestHits("POST");
  return visitRequest;
}

function createEntries(
  lines: OutputLine[],
  idRef: { current: number },
): TerminalEntry[] {
  return lines.map((output) => ({
    id: idRef.current++,
    type: "output" as const,
    line: output,
  }));
}

function getWelcomeLines(): OutputLine[] {
  return [
    {
      text: "Nivalis 1.0 — guest session",
      tone: "accent",
    },
    {
      text: "A quiet Unix shell at the edge of the network.",
      tone: "default",
    },
    {
      text: "Type 'help' to explore. This filesystem is read-only.",
      tone: "muted",
    },
    { text: "" },
    { text: "" },
  ];
}

export function Terminal() {
  const nextId = useRef(1);
  const inputRef = useRef<HTMLInputElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const surfaceRef = useRef<HTMLDivElement>(null);
  const sessionStartedAt = useRef(Date.now());
  const [entries, setEntries] = useState<TerminalEntry[]>([]);
  const [input, setInput] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [historyIndex, setHistoryIndex] = useState(0);
  const [cwd, setCwd] = useState("/home/guest");
  const [theme, setTheme] = useState<ThemeName>("nivalis");
  const [visitCount, setVisitCount] = useState<number | null>(null);
  const [booting, setBooting] = useState(true);
  const [completion, setCompletion] = useState<CdCompletionSession | null>(
    null,
  );

  const focusInput = useCallback(() => {
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, []);

  const boot = useCallback(() => {
    setBooting(true);
    setEntries([]);
    setCwd("/home/guest");
    setInput("");
    setCompletion(null);
    sessionStartedAt.current = Date.now();

    const reducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    const timer = window.setTimeout(
      () => {
        setEntries(createEntries(getWelcomeLines(), nextId));
        setBooting(false);
        focusInput();
      },
      reducedMotion ? 0 : 180,
    );

    return () => window.clearTimeout(timer);
  }, [focusInput]);

  useEffect(() => boot(), [boot]);

  useEffect(() => {
    let active = true;

    void recordVisit()
      .then((count) => {
        if (active) setVisitCount(count);
      })
      .catch(() => {
        // The shell remains usable if the edge counter is unavailable.
      });

    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    const viewport = scrollRef.current;
    if (!viewport) return;
    viewport.scrollTop = viewport.scrollHeight;
  }, [entries, booting, completion]);

  useEffect(() => {
    const surface = surfaceRef.current;
    if (!surface) return;

    const handleWheel = (event: WheelEvent) => {
      const viewport = scrollRef.current;
      if (!viewport || viewport.scrollHeight <= viewport.clientHeight) return;

      const scale =
        event.deltaMode === WheelEvent.DOM_DELTA_LINE
          ? 16
          : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
            ? viewport.clientHeight
            : 1;
      const nextTop = Math.max(
        0,
        Math.min(
          viewport.scrollHeight - viewport.clientHeight,
          viewport.scrollTop + event.deltaY * scale,
        ),
      );

      if (nextTop !== viewport.scrollTop) {
        viewport.scrollTop = nextTop;
        event.preventDefault();
        event.stopPropagation();
      }
    };

    surface.addEventListener("wheel", handleWheel, { passive: false });
    return () => surface.removeEventListener("wheel", handleWheel);
  }, []);

  const execute = useCallback(
    (rawCommand: string) => {
      if (booting) return;

      setCompletion(null);
      const command = rawCommand.trim();
      const commandEntry: TerminalEntry = {
        id: nextId.current++,
        type: "command",
        command,
        cwd,
      };

      if (!command) {
        setEntries((current) => [...current, commandEntry]);
        setInput("");
        return;
      }

      const nextHistory = [...history, command];
      const result = runCommand(command, {
        cwd,
        history: nextHistory,
        sessionStartedAt: sessionStartedAt.current,
      });

      if (result.action === "clear") {
        setEntries([]);
      } else if (result.action === "reboot") {
        setEntries((current) => [...current, commandEntry]);
        window.setTimeout(() => boot(), 140);
      } else if (result.action === "hits") {
        const outputId = nextId.current++;
        setEntries((current) => [
          ...current,
          commandEntry,
          {
            id: outputId,
            type: "output",
            line: { text: "reading counter…", tone: "muted" },
          },
        ]);

        void requestHits("GET")
          .then((count) => {
            setVisitCount(count);
            setEntries((current) =>
              current.map((entry) =>
                entry.id === outputId
                  ? {
                      id: outputId,
                      type: "output" as const,
                      line: {
                        text: `${count.toLocaleString("en-US")} visits`,
                        tone: "default" as const,
                      },
                    }
                  : entry,
              ),
            );
          })
          .catch(() => {
            setEntries((current) =>
              current.map((entry) =>
                entry.id === outputId
                  ? {
                      id: outputId,
                      type: "output" as const,
                      line: {
                        text:
                          visitCount === null
                            ? "hits: counter unavailable"
                            : `${visitCount.toLocaleString("en-US")} visits (cached)`,
                        tone: visitCount === null ? "error" : "warning",
                      },
                    }
                  : entry,
              ),
            );
          });
      } else {
        setEntries((current) => [
          ...current,
          commandEntry,
          ...createEntries(result.lines ?? [], nextId),
        ]);
      }

      if (result.cwd) setCwd(result.cwd);
      if (result.theme) setTheme(result.theme);
      setHistory(nextHistory);
      setHistoryIndex(nextHistory.length);
      setInput("");
    },
    [boot, booting, cwd, history, visitCount],
  );

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault();
    execute(input);
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setCompletion(null);
      if (!history.length) return;
      const nextIndex = Math.max(0, historyIndex - 1);
      setHistoryIndex(nextIndex);
      setInput(history[nextIndex] ?? "");
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();
      setCompletion(null);
      const nextIndex = Math.min(history.length, historyIndex + 1);
      setHistoryIndex(nextIndex);
      setInput(nextIndex === history.length ? "" : history[nextIndex] ?? "");
      return;
    }

    if (event.key === "Tab") {
      event.preventDefault();

      const direction = event.shiftKey ? -1 : 1;
      if (completion) {
        const nextIndex =
          (completion.index + direction + completion.candidates.length) %
          completion.candidates.length;
        setCompletion({ ...completion, index: nextIndex });
        setInput(completion.candidates[nextIndex].value);
        return;
      }

      const candidates = getCdCompletionCandidates(input, cwd);
      if (candidates.length > 0) {
        const index = event.shiftKey ? candidates.length - 1 : 0;
        setCompletion({ candidates, index });
        setInput(candidates[index].value);
        return;
      }

      setInput((current) => completeInput(current, cwd));
      return;
    }

    if (event.key === "Escape" && completion) {
      event.preventDefault();
      setCompletion(null);
      return;
    }

    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "l") {
      event.preventDefault();
      setCompletion(null);
      setEntries([]);
      return;
    }

    if (event.ctrlKey && event.key.toLowerCase() === "c") {
      event.preventDefault();
      setCompletion(null);
      setEntries((current) => [
        ...current,
        {
          id: nextId.current++,
          type: "command",
          command: `${input}^C`,
          cwd,
        },
      ]);
      setInput("");
      return;
    }

    if (event.ctrlKey && event.key.toLowerCase() === "u") {
      event.preventDefault();
      setCompletion(null);
      setInput("");
    }
  };

  const promptPath = useMemo(() => displayPath(cwd), [cwd]);

  return (
    <section
      className="terminal-app"
      data-terminal-theme={theme}
      aria-label="Nivalis Unix terminal"
    >
      <header className="terminal-titlebar">
        <div className="traffic-lights" aria-hidden="true">
          <span className="traffic-light close" />
          <span className="traffic-light minimize" />
          <span className="traffic-light maximize" />
        </div>

        <div className="terminal-title">
          <span>guest@nivalis — {promptPath} — zsh</span>
        </div>

        <div aria-hidden="true" />
      </header>

      <div
        ref={surfaceRef}
        className="terminal-surface"
        onClick={focusInput}
      >
        <div
          className="terminal-viewport"
          ref={scrollRef}
          role="log"
          aria-live="polite"
          aria-label="Terminal output"
          tabIndex={0}
        >
          <div className="terminal-content">
            {entries.map((entry) => {
              if (entry.type === "command") {
                return (
                  <div className="command-row" key={entry.id}>
                    <Prompt cwd={entry.cwd} />
                    <span className="command-copy">{entry.command}</span>
                  </div>
                );
              }

              return (
                <div
                  className={`output-line tone-${entry.line.tone ?? "default"}${entry.line.preserve ? " preserve" : ""}`}
                  key={entry.id}
                >
                  {entry.line.href ? (
                    <a
                      className="terminal-link"
                      href={entry.line.href}
                      target="_blank"
                      rel="noopener noreferrer"
                    >
                      {entry.line.text}
                    </a>
                  ) : (
                    entry.line.text || "\u00A0"
                  )}
                </div>
              );
            })}

            {!booting && (
              <form className="input-row" onSubmit={handleSubmit}>
                <Prompt cwd={cwd} />
                <label className="input-wrap">
                  <span className="sr-only">Enter a shell command</span>
                  <input
                    ref={inputRef}
                    id="terminal-command"
                    name="command"
                    value={input}
                    onChange={(event) => {
                      setCompletion(null);
                      setInput(event.target.value);
                    }}
                    onKeyDown={handleKeyDown}
                    autoCapitalize="none"
                    autoComplete="off"
                    autoCorrect="off"
                    spellCheck={false}
                    role="combobox"
                    aria-autocomplete="list"
                    aria-controls={
                      completion ? COMPLETION_LIST_ID : undefined
                    }
                    aria-expanded={completion !== null}
                    aria-activedescendant={
                      completion
                        ? `${COMPLETION_LIST_ID}-${completion.index}`
                        : undefined
                    }
                    aria-label={`Command input at guest@nivalis:${promptPath}`}
                  />
                </label>
              </form>
            )}

            {completion && (
              <div
                className="completion-menu"
                id={COMPLETION_LIST_ID}
                role="listbox"
                aria-label="Available directories"
                aria-orientation="horizontal"
              >
                {completion.candidates.map((candidate, index) => {
                  const active = index === completion.index;
                  return (
                    <span
                      className={`completion-option${active ? " is-active" : ""}`}
                      id={`${COMPLETION_LIST_ID}-${index}`}
                      key={candidate.value}
                      role="option"
                      aria-selected={active}
                    >
                      <span className="completion-marker" aria-hidden="true">
                        ›
                      </span>
                      {candidate.label}
                    </span>
                  );
                })}
                <span className="completion-hint" aria-hidden="true">
                  tab next · ⇧tab previous · enter open
                </span>
              </div>
            )}
          </div>
        </div>
      </div>
    </section>
  );
}

function Prompt({ cwd }: { cwd: string }) {
  return (
    <span className="prompt" aria-hidden="true">
      <span className="prompt-user">guest</span>
      <span className="prompt-dim">@</span>
      <span className="prompt-host">nivalis</span>
      <span className="prompt-dim">:</span>
      <span className="prompt-path">{displayPath(cwd)}</span>
      <span className="prompt-mark">%</span>
    </span>
  );
}
