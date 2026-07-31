import {
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";

export interface CustomSelectOption<T extends string> {
  value: T;
  label: string;
}

interface CustomSelectProps<T extends string> {
  value: T;
  options: readonly CustomSelectOption<T>[];
  onChange: (value: T) => void;
  ariaLabel: string;
  className?: string;
}

export function CustomSelect<T extends string>({
  value,
  options,
  onChange,
  ariaLabel,
  className = "",
}: CustomSelectProps<T>) {
  const listboxId = useId();
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const selectedIndex = Math.max(
    0,
    options.findIndex((option) => option.value === value),
  );
  const [isOpen, setIsOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(selectedIndex);
  const selectedOption = options[selectedIndex];

  useEffect(() => {
    if (!isOpen) return;
    optionRefs.current[activeIndex]?.focus();
  }, [activeIndex, isOpen]);

  useEffect(() => {
    const closeOnOutsidePointer = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !rootRef.current?.contains(event.target)
      ) {
        setIsOpen(false);
      }
    };
    document.addEventListener("pointerdown", closeOnOutsidePointer);
    return () =>
      document.removeEventListener("pointerdown", closeOnOutsidePointer);
  }, []);

  const open = (index = selectedIndex) => {
    setActiveIndex(index);
    setIsOpen(true);
  };

  const close = (restoreFocus = false) => {
    setIsOpen(false);
    if (restoreFocus) triggerRef.current?.focus();
  };

  const choose = (index: number) => {
    const option = options[index];
    if (!option) return;
    onChange(option.value);
    close(true);
  };

  const moveActive = (direction: 1 | -1) => {
    setActiveIndex((current) => {
      const next = current + direction;
      if (next < 0) return options.length - 1;
      if (next >= options.length) return 0;
      return next;
    });
  };

  const onTriggerKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      open(selectedIndex);
      return;
    }
    if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      open(event.key === "Home" ? 0 : options.length - 1);
    }
  };

  const onOptionKeyDown = (
    event: KeyboardEvent<HTMLButtonElement>,
    index: number,
  ) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      moveActive(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      setActiveIndex(event.key === "Home" ? 0 : options.length - 1);
      return;
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      choose(index);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      close(true);
      return;
    }
    if (event.key === "Tab") {
      close();
    }
  };

  return (
    <div
      ref={rootRef}
      className={`custom-select ${isOpen ? "is-open" : ""} ${className}`.trim()}
    >
      <button
        ref={triggerRef}
        type="button"
        className="custom-select-trigger"
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={isOpen}
        aria-controls={isOpen ? listboxId : undefined}
        onClick={() => {
          if (isOpen) close();
          else open();
        }}
        onKeyDown={onTriggerKeyDown}
      >
        <span>{selectedOption?.label ?? ""}</span>
        <span className="material-symbols-outlined" aria-hidden="true">
          expand_more
        </span>
      </button>

      {isOpen ? (
        <div
          id={listboxId}
          className="custom-select-menu"
          role="listbox"
          aria-label={ariaLabel}
        >
          {options.map((option, index) => {
            const selected = option.value === value;
            return (
              <button
                key={option.value}
                ref={(element) => {
                  optionRefs.current[index] = element;
                }}
                type="button"
                role="option"
                aria-selected={selected}
                className={`custom-select-option ${selected ? "is-selected" : ""}`}
                tabIndex={index === activeIndex ? 0 : -1}
                onFocus={() => setActiveIndex(index)}
                onClick={() => choose(index)}
                onKeyDown={(event) => onOptionKeyDown(event, index)}
              >
                <span>{option.label}</span>
                <span
                  className="material-symbols-outlined"
                  aria-hidden="true"
                >
                  {selected ? "check" : ""}
                </span>
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
