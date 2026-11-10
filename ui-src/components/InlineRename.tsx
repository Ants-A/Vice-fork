import {useRef, useState} from 'react';

/**
 * The rename-in-place field, in one component.
 *
 * There were two of these with slightly different behaviour, on the clip card
 * and on a highlight row, and renaming from the viewer and the trim window
 * would have made four. The differences that mattered are the two props below;
 * everything else about them was already the same.
 *
 * Blur commits, because the surfaces this appears on close on an outside
 * click and an edit that vanished on the way out reads as data loss. The
 * `done` guard is what stops the blur that follows Enter from submitting a
 * second time.
 */
export function InlineRename({
  initial,
  onSubmit,
  onCancel,
  className,
  label,
  emptyFallback,
  stopPropagation,
  literal = false,
}: {
  initial: string;
  onSubmit: (name: string) => void;
  onCancel: () => void;
  className: string;
  label: string;
  /** Used when the field is emptied. Without one, an empty name cancels. */
  emptyFallback?: string;
  /** For a field inside something that is itself clickable, like a list row. */
  stopPropagation?: boolean;
  /** File names keep all entered characters, including leading/trailing spaces. */
  literal?: boolean;
}) {
  const [value, setValue] = useState(initial);
  const done = useRef(false);

  const submit = () => {
    if (done.current) return;
    done.current = true;
    const next = (literal ? value : value.trim()) || emptyFallback || '';
    if (!next || next === initial) onCancel();
    else onSubmit(next);
  };

  return (
    <input
      className={className}
      value={value}
      autoFocus
      aria-label={label}
      onClick={stopPropagation ? e => e.stopPropagation() : undefined}
      onChange={e => setValue(e.target.value)}
      onFocus={e => e.target.select()}
      onBlur={submit}
      onKeyDown={e => {
        if (stopPropagation) e.stopPropagation();
        if (e.key === 'Enter') {
          e.preventDefault();
          submit();
        }
        if (e.key === 'Escape') {
          // Marked done first: cancelling still blurs, and the blur handler
          // would otherwise commit the edit the user just abandoned.
          done.current = true;
          onCancel();
        }
      }}
    />
  );
}
