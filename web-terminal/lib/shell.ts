export type Tone =
  | "default"
  | "muted"
  | "accent"
  | "success"
  | "warning"
  | "error"
  | "ice"
  | "directory";

export type OutputLine = {
  text: string;
  tone?: Tone;
  preserve?: boolean;
  href?: string;
};

export type ThemeName = "nivalis" | "noir" | "ember";

export type CdCompletionCandidate = {
  label: string;
  value: string;
};

export type ShellContext = {
  cwd: string;
  history: string[];
  sessionStartedAt: number;
};

export type CommandResult = {
  lines?: OutputLine[];
  cwd?: string;
  action?: "clear" | "reboot" | "hits";
  theme?: ThemeName;
};

type FileEntry = {
  kind: "file";
  content: string;
};

type DirectoryEntry = {
  kind: "dir";
};

type FsEntry = FileEntry | DirectoryEntry;

const FILE_SYSTEM: Record<string, FsEntry> = {
  "/": { kind: "dir" },
  "/home": { kind: "dir" },
  "/home/guest": { kind: "dir" },
  "/home/guest/projects": { kind: "dir" },
  "/home/guest/.config": { kind: "dir" },
  "/etc": { kind: "dir" },
  "/var": { kind: "dir" },
  "/var/log": { kind: "dir" },
  "/home/guest/README.md": {
    kind: "file",
    content: [
      "# Nivalis",
      "",
      "A quiet terminal at the edge of the network.",
      "Type `help` to discover the available commands.",
      "",
      "This is a sandboxed browser shell. No command reaches your device.",
    ].join("\n"),
  },
  "/home/guest/about.txt": {
    kind: "file",
    content: [
      "Nivalis is a small digital outpost built around clarity,",
      "craft, and a healthy respect for the command line.",
      "",
      "host    terminal.nivalis.is",
      "user    guest",
      "shell   /bin/zsh",
    ].join("\n"),
  },
  "/home/guest/projects/terminal.txt": {
    kind: "file",
    content: [
      "project: web-terminal",
      "status:  online",
      "stack:   Next.js / TypeScript / CSS",
      "mode:    read-only guest sandbox",
    ].join("\n"),
  },
  "/home/guest/projects/aurora.log": {
    kind: "file",
    content: [
      "[ok] northern relay initialized",
      "[ok] packet snowfield calibrated",
      "[ok] aurora channel listening",
    ].join("\n"),
  },
  "/home/guest/.config/profile": {
    kind: "file",
    content: [
      "export USER=guest",
      "export HOSTNAME=nivalis",
      "export SHELL=/bin/zsh",
      "export LANG=en_US.UTF-8",
    ].join("\n"),
  },
  "/etc/hostname": {
    kind: "file",
    content: "nivalis",
  },
  "/etc/os-release": {
    kind: "file",
    content: [
      'NAME="Nivalis Linux"',
      'PRETTY_NAME="Nivalis Linux 1.0 (Aurora)"',
      'ID="nivalis"',
      'VERSION_CODENAME="aurora"',
      'HOME_URL="https://nivalis.is"',
    ].join("\n"),
  },
  "/var/log/boot.log": {
    kind: "file",
    content: [
      "[    0.021] nivalis kernel initialized",
      "[    0.144] mounted guest filesystem (read-only)",
      "[    0.319] started remote shell",
      "[    0.421] listening on tty1",
    ].join("\n"),
  },
};

export const COMMANDS = [
  "help",
  "about",
  "whoami",
  "hostname",
  "uname",
  "id",
  "date",
  "uptime",
  "pwd",
  "ls",
  "cd",
  "cat",
  "echo",
  "env",
  "tree",
  "history",
  "clear",
  "neofetch",
  "status",
  "hits",
  "github",
  "sites",
  "theme",
  "fortune",
  "ping",
  "man",
  "sudo",
  "reboot",
  "exit",
] as const;

const HELP_ROWS = [
  ["about", "read the Nivalis field notes"],
  ["ls [-la]", "list directory contents"],
  ["cd <path>", "change the working directory"],
  ["cat <file>", "print a file"],
  ["pwd", "show the current directory"],
  ["whoami", "print the current user"],
  ["neofetch", "show system information"],
  ["status", "inspect Nivalis services"],
  ["hits", "show terminal visit count"],
  ["github", "open myouo on GitHub"],
  ["sites", "list myouo's websites"],
  ["theme <name>", "use nivalis, noir, or ember"],
  ["history", "show command history"],
  ["clear", "clear the terminal"],
  ["reboot", "replay the boot sequence"],
];

const MANUALS: Record<string, string[]> = {
  ls: [
    "LS(1)                       Nivalis Manual",
    "",
    "NAME",
    "    ls — list directory contents",
    "",
    "SYNOPSIS",
    "    ls [-a] [-l] [path]",
  ],
  cd: [
    "CD(1)                       Nivalis Manual",
    "",
    "NAME",
    "    cd — change the shell working directory",
    "",
    "SYNOPSIS",
    "    cd [directory]",
  ],
  cat: [
    "CAT(1)                      Nivalis Manual",
    "",
    "NAME",
    "    cat — print files to standard output",
    "",
    "SYNOPSIS",
    "    cat <file>...",
  ],
  theme: [
    "THEME(1)                    Nivalis Manual",
    "",
    "NAME",
    "    theme — change the terminal color profile",
    "",
    "SYNOPSIS",
    "    theme [nivalis|noir|ember]",
  ],
  hits: [
    "HITS(1)                     Nivalis Manual",
    "",
    "NAME",
    "    hits — show the terminal visit count",
    "",
    "SYNOPSIS",
    "    hits",
  ],
  github: [
    "GITHUB(1)                   Nivalis Manual",
    "",
    "NAME",
    "    github — open myouo on GitHub",
    "",
    "SYNOPSIS",
    "    github",
  ],
  sites: [
    "SITES(1)                    Nivalis Manual",
    "",
    "NAME",
    "    sites — list myouo's websites",
    "",
    "SYNOPSIS",
    "    sites",
  ],
};

const GITHUB_URL = "https://github.com/myouo";

const WEB_SITES = [
  ["terminal", "https://terminal.nivalis.is"],
  ["images", "https://img.nivalis.is"],
  ["timer", "https://timer.nivalis.is"],
] as const;

const FORTUNES = [
  "The quieter the terminal, the louder the signal.",
  "A clean prompt is a small form of optimism.",
  "Measure twice. Pipe once.",
  "There is no cloud. It is just someone else's weather.",
];

function line(text: string, tone: Tone = "default", preserve = false): OutputLine {
  return { text, tone, preserve };
}

function link(
  text: string,
  href: string,
  tone: Tone = "ice",
  preserve = false,
): OutputLine {
  return { text, tone, preserve, href };
}

function tokenize(input: string): string[] {
  const tokens: string[] = [];
  const pattern = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let match: RegExpExecArray | null;

  while ((match = pattern.exec(input)) !== null) {
    tokens.push(match[1] ?? match[2] ?? match[3]);
  }

  return tokens;
}

function normalizePath(path: string): string {
  const pieces = path.split("/");
  const clean: string[] = [];

  for (const piece of pieces) {
    if (!piece || piece === ".") continue;
    if (piece === "..") clean.pop();
    else clean.push(piece);
  }

  return `/${clean.join("/")}`;
}

export function resolvePath(input: string | undefined, cwd: string): string {
  if (!input || input === "~") return "/home/guest";
  const expanded = input.startsWith("~")
    ? input.replace("~", "/home/guest")
    : input;
  return normalizePath(expanded.startsWith("/") ? expanded : `${cwd}/${expanded}`);
}

export function displayPath(path: string): string {
  if (path === "/home/guest") return "~";
  if (path.startsWith("/home/guest/")) {
    return path.replace("/home/guest", "~");
  }
  return path;
}

function directoryChildren(path: string, includeHidden: boolean) {
  const prefix = path === "/" ? "/" : `${path}/`;
  return Object.keys(FILE_SYSTEM)
    .filter((entry) => {
      if (!entry.startsWith(prefix) || entry === path) return false;
      const remainder = entry.slice(prefix.length);
      if (remainder.includes("/")) return false;
      return includeHidden || !remainder.startsWith(".");
    })
    .map((entry) => ({
      path: entry,
      name: entry.slice(prefix.length),
      ...FILE_SYSTEM[entry],
    }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

function formatLongEntry(
  entry: ReturnType<typeof directoryChildren>[number],
): string {
  const isDirectory = entry.kind === "dir";
  const mode = isDirectory ? "dr-xr-xr-x" : "-r--r--r--";
  const size =
    entry.kind === "file"
      ? new TextEncoder().encode(entry.content).length.toString().padStart(4)
      : "4096";
  return `${mode}  1 guest guest ${size} Jul 29 08:48 ${entry.name}${isDirectory ? "/" : ""}`;
}

function expandVariables(value: string) {
  return value
    .replaceAll("$USER", "guest")
    .replaceAll("$HOME", "/home/guest")
    .replaceAll("$SHELL", "/bin/zsh")
    .replaceAll("$HOSTNAME", "nivalis");
}

function formatDuration(milliseconds: number) {
  const totalSeconds = Math.max(1, Math.floor(milliseconds / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}m ${seconds}s`;
}

function tree(path: string, depth = 0): string[] {
  const children = directoryChildren(path, false);
  const output: string[] = [];

  children.forEach((entry, index) => {
    const last = index === children.length - 1;
    output.push(
      `${"│   ".repeat(depth)}${last ? "└── " : "├── "}${entry.name}${entry.kind === "dir" ? "/" : ""}`,
    );
    if (entry.kind === "dir") {
      output.push(...tree(entry.path, depth + 1));
    }
  });

  return output;
}

export function runCommand(
  rawInput: string,
  context: ShellContext,
): CommandResult {
  const tokens = tokenize(rawInput.trim());
  if (tokens.length === 0) return {};

  const [rawCommand, ...args] = tokens;
  const command = rawCommand;

  switch (command) {
    case "help":
      return {
        lines: [
          line("AVAILABLE COMMANDS", "accent"),
          line(""),
          ...HELP_ROWS.map(([name, detail]) =>
            line(`${name.padEnd(16)} ${detail}`, "default", true),
          ),
          line(""),
          line("Tip: use ↑/↓ for history, Tab to complete, Ctrl+L to clear.", "muted"),
        ],
      };

    case "about": {
      const about = FILE_SYSTEM["/home/guest/about.txt"] as FileEntry;
      return {
        lines: about.content.split("\n").map((text) => line(text)),
      };
    }

    case "whoami":
      return { lines: [line("guest")] };

    case "hostname":
      return { lines: [line("nivalis")] };

    case "uname": {
      const all = args.includes("-a");
      return {
        lines: [
          line(
            all
              ? "Nivalis nivalis 6.12.7-nvl #1 SMP PREEMPT_DYNAMIC x86_64 GNU/Linux"
              : "Nivalis",
          ),
        ],
      };
    }

    case "id":
      return {
        lines: [
          line("uid=1000(guest) gid=1000(guest) groups=1000(guest),20(dialout)"),
        ],
      };

    case "date":
      return {
        lines: [
          line(
            new Intl.DateTimeFormat("en-GB", {
              weekday: "short",
              day: "2-digit",
              month: "short",
              year: "numeric",
              hour: "2-digit",
              minute: "2-digit",
              second: "2-digit",
              hour12: false,
              timeZoneName: "short",
            }).format(new Date()),
          ),
        ],
      };

    case "uptime":
      return {
        lines: [
          line(
            `up ${formatDuration(Date.now() - context.sessionStartedAt)}, 1 user, load average: 0.08, 0.04, 0.01`,
          ),
        ],
      };

    case "pwd":
      return { lines: [line(context.cwd)] };

    case "ls": {
      const flags = args.filter((arg) => arg.startsWith("-")).join("");
      const pathArg = args.find((arg) => !arg.startsWith("-"));
      const path = resolvePath(pathArg ?? ".", context.cwd);
      const entry = FILE_SYSTEM[path];

      if (!entry) {
        return {
          lines: [line(`ls: cannot access '${pathArg ?? "."}': No such file or directory`, "error")],
        };
      }

      if (entry.kind === "file") {
        return { lines: [line(path.split("/").pop() ?? path)] };
      }

      const children = directoryChildren(path, flags.includes("a"));
      if (flags.includes("l")) {
        return {
          lines: [
            line(`total ${children.length * 4}`, "muted"),
            ...children.map((child) =>
              line(
                formatLongEntry(child),
                child.kind === "dir" ? "directory" : "default",
                true,
              ),
            ),
          ],
        };
      }

      return {
        lines: children.length
          ? children.map((child) =>
              line(
                `${child.name}${child.kind === "dir" ? "/" : ""}`,
                child.kind === "dir" ? "directory" : "default",
              ),
            )
          : [line("")],
      };
    }

    case "cd": {
      const path = resolvePath(args[0], context.cwd);
      const entry = FILE_SYSTEM[path];

      if (!entry) {
        return {
          lines: [line(`cd: ${args[0]}: No such file or directory`, "error")],
        };
      }
      if (entry.kind !== "dir") {
        return {
          lines: [line(`cd: ${args[0]}: Not a directory`, "error")],
        };
      }
      return { cwd: path };
    }

    case "cat": {
      if (!args.length) {
        return { lines: [line("cat: missing file operand", "error")] };
      }

      const output: OutputLine[] = [];
      args.forEach((arg) => {
        const path = resolvePath(arg, context.cwd);
        const entry = FILE_SYSTEM[path];
        if (!entry) {
          output.push(line(`cat: ${arg}: No such file or directory`, "error"));
        } else if (entry.kind === "dir") {
          output.push(line(`cat: ${arg}: Is a directory`, "error"));
        } else {
          output.push(...entry.content.split("\n").map((text) => line(text)));
        }
      });
      return { lines: output };
    }

    case "echo":
      return { lines: [line(expandVariables(args.join(" ")))] };

    case "env":
      return {
        lines: [
          line("USER=guest"),
          line("HOME=/home/guest"),
          line("SHELL=/bin/zsh"),
          line("HOSTNAME=nivalis"),
          line("LANG=en_US.UTF-8"),
          line("TERM=xterm-256color"),
          line("PATH=/usr/local/bin:/usr/bin:/bin"),
        ],
      };

    case "tree": {
      const path = resolvePath(args[0] ?? ".", context.cwd);
      const entry = FILE_SYSTEM[path];
      if (!entry) {
        return { lines: [line(`tree: ${args[0]}: No such file or directory`, "error")] };
      }
      if (entry.kind === "file") {
        return { lines: [line(args[0] ?? path)] };
      }
      const output = [line(displayPath(path), "directory")];
      const branches = tree(path);
      return {
        lines: [
          ...output,
          ...branches.map((text) =>
            line(text, text.endsWith("/") ? "directory" : "default", true),
          ),
          line(""),
          line(`${branches.filter((branch) => branch.endsWith("/")).length} directories, ${branches.filter((branch) => !branch.endsWith("/")).length} files`, "muted"),
        ],
      };
    }

    case "history":
      return {
        lines: context.history.map((entry, index) =>
          line(`${String(index + 1).padStart(4)}  ${entry}`, "default", true),
        ),
      };

    case "clear":
      return { action: "clear" };

    case "neofetch":
      return {
        lines: [
          line("          *          guest@nivalis", "ice", true),
          line("       * /|\\ *       -------------", "ice", true),
          line("     *  /_|_\\  *     OS: Nivalis Linux 1.0", "ice", true),
          line("       /  |  \\       Host: terminal.nivalis.is", "ice", true),
          line("    __/___|___\\__    Kernel: 6.12.7-nvl", "ice", true),
          line("       \\  |  /       Shell: zsh 5.9", "ice", true),
          line("        \\ | /        Terminal: web-tty", "ice", true),
          line("          *          Theme: Nivalis", "ice", true),
          line("                     Memory: 64 MiB / ∞", "ice", true),
        ],
      };

    case "status":
      return {
        lines: [
          line("SERVICE                STATE       LATENCY", "accent", true),
          line("remote-shell           active      12ms", "success", true),
          line("northern-relay         active      18ms", "success", true),
          line("guest-filesystem       read-only   <1ms", "ice", true),
          line("aurora-channel         listening   7ms", "success", true),
          line(""),
          line("All systems nominal.", "success"),
        ],
      };

    case "hits":
      return { action: "hits" };

    case "github":
      return {
        lines: [
          line("GitHub · myouo", "accent"),
          link(GITHUB_URL, GITHUB_URL),
        ],
      };

    case "sites":
      return {
        lines: [
          line("WEB SITES", "accent"),
          line(""),
          ...WEB_SITES.map(([name, url]) =>
            link(`${name.padEnd(12)} ${url}`, url, "ice", true),
          ),
          line(""),
          line(`${WEB_SITES.length} sites`, "muted"),
        ],
      };

    case "theme": {
      const requested = (args[0] ?? "") as ThemeName;
      if (!args[0]) {
        return {
          lines: [line("Available themes: nivalis, noir, ember", "muted")],
        };
      }
      if (!["nivalis", "noir", "ember"].includes(requested)) {
        return {
          lines: [line(`theme: unknown profile '${args[0]}'`, "error")],
        };
      }
      return {
        theme: requested,
        lines: [line(`Theme profile '${requested}' loaded.`, "success")],
      };
    }

    case "fortune":
      return {
        lines: [
          line(
            FORTUNES[
              context.history.join("").length % FORTUNES.length
            ],
            "accent",
          ),
        ],
      };

    case "ping": {
      const target = args[0] ?? "nivalis";
      return {
        lines: [
          line(`PING ${target} (${target === "nivalis" ? "127.0.0.1" : "203.0.113.7"}): 56 data bytes`),
          line(`64 bytes from ${target}: icmp_seq=0 ttl=64 time=12.4 ms`),
          line(`64 bytes from ${target}: icmp_seq=1 ttl=64 time=11.8 ms`),
          line(""),
          line(`--- ${target} ping statistics ---`),
          line("2 packets transmitted, 2 packets received, 0.0% packet loss"),
        ],
      };
    }

    case "man": {
      const subject = args[0];
      if (!subject) {
        return { lines: [line("What manual page do you want?", "warning")] };
      }
      const manual = MANUALS[subject];
      if (!manual) {
        return { lines: [line(`No manual entry for ${subject}`, "error")] };
      }
      return {
        lines: manual.map((text) => line(text, "default", true)),
      };
    }

    case "sudo":
      return {
        lines: [
          line("guest is not in the sudoers file. This incident will be quietly ignored.", "warning"),
        ],
      };

    case "reboot":
      return { action: "reboot" };

    case "exit":
    case "logout":
      return {
        lines: [
          line("logout", "muted"),
          line("Session preserved — this terminal is a browser sandbox.", "accent"),
        ],
      };

    case "rm":
    case "mv":
    case "cp":
    case "touch":
    case "mkdir":
      return {
        lines: [
          line(`${command}: filesystem is read-only for guest`, "warning"),
        ],
      };

    default:
      return {
        lines: [
          line(`${rawCommand}: command not found`, "error"),
          line("Type 'help' to list available commands.", "muted"),
        ],
      };
  }
}

export function getCdCompletionCandidates(
  value: string,
  cwd: string,
): CdCompletionCandidate[] {
  const match = /^cd\s+(\S*)$/.exec(value);
  if (!match) return [];

  const path = match[1];
  const slashIndex = path.lastIndexOf("/");
  const directoryPart = slashIndex >= 0 ? path.slice(0, slashIndex + 1) : "";
  const namePart = slashIndex >= 0 ? path.slice(slashIndex + 1) : path;
  const directory = resolvePath(directoryPart || ".", cwd);

  return directoryChildren(directory, true)
    .filter(
      (entry) => entry.kind === "dir" && entry.name.startsWith(namePart),
    )
    .map((entry) => {
      const completedPath = `${directoryPart}${entry.name}/`;
      return {
        label: completedPath,
        value: `cd ${completedPath}`,
      };
    });
}

export function completeInput(value: string, cwd: string): string {
  const parts = value.split(/\s+/);
  const current = parts.at(-1) ?? "";
  const command = parts[0] ?? "";

  if (parts.length === 1) {
    const matches = COMMANDS.filter((candidate) =>
      candidate.startsWith(current),
    );
    if (matches.length === 1) return `${matches[0]} `;
    return value;
  }

  if (command === "cd" && parts.length > 2) return value;

  const path = current || ".";
  const slashIndex = path.lastIndexOf("/");
  const directoryPart = slashIndex >= 0 ? path.slice(0, slashIndex + 1) : "";
  const namePart = slashIndex >= 0 ? path.slice(slashIndex + 1) : path;
  const directory = resolvePath(directoryPart || ".", cwd);
  const matches = directoryChildren(directory, true).filter(
    (entry) =>
      entry.name.startsWith(namePart) &&
      (command !== "cd" || entry.kind === "dir"),
  );

  if (matches.length !== 1) return value;
  const match = matches[0];
  parts[parts.length - 1] =
    `${directoryPart}${match.name}${match.kind === "dir" ? "/" : " "}`;
  return parts.join(" ");
}
