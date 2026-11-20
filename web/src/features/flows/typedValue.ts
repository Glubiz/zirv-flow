import { useState } from "react";

/**
 * A field that keeps what is typed while it is being typed, and commits only what `parse` accepts.
 * `parse` returns the value for the text, or `null` when the text is not yet a value, which saves
 * nothing and sets `bad`. A change of `value` from outside, such as an undo, replaces the text
 * unless the text still reads as that value.
 */
export function useTypedValue<T>(value: T, parse: (text: string) => T | null, show: (value: T) => string, commit: (value: T) => void) {
  const [text, setText] = useState(() => show(value));
  const [seen, setSeen] = useState(value);
  const [bad, setBad] = useState(false);
  if (!Object.is(seen, value)) {
    setSeen(value);
    if (bad || !Object.is(parse(text), value)) {
      setText(show(value));
      setBad(false);
    }
  }

  function change(next: string) {
    setText(next);
    const parsed = parse(next);
    if (parsed === null) {
      setBad(true);
      return;
    }
    setBad(false);
    setSeen(parsed);
    if (!Object.is(parsed, value)) commit(parsed);
  }

  return { text, bad, change };
}
