/** The field a merge tag from the picker goes into: the last text field the marketer focused. */
export interface InsertTarget {
  label: string;
  insert: (tag: string) => void;
}

/**
 * A target for a plain input or textarea. The tag is put in through the browser's own value setter
 * and an `input` event, so the field's change handler sees it like typing and works on the newest
 * state rather than the state at focus time.
 */
export function fieldTarget(label: string, element: HTMLInputElement | HTMLTextAreaElement): InsertTarget {
  return {
    label,
    insert: (tag) => {
      if (!element.isConnected) return;
      const start = element.selectionStart ?? element.value.length;
      const end = element.selectionEnd ?? start;
      const descriptor = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(element) as object, "value");
      const next = element.value.slice(0, start) + tag + element.value.slice(end);
      if (descriptor?.set) descriptor.set.call(element, next);
      else element.value = next;
      element.dispatchEvent(new Event("input", { bubbles: true }));
      element.focus();
      const caret = start + tag.length;
      element.setSelectionRange(caret, caret);
    },
  };
}
