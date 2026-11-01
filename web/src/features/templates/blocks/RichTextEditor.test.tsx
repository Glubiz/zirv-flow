import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { RichTextEditor } from "./RichTextEditor";
import { cleanRichText, linkAllowed, richTextToEditor } from "./richText";

const PAYLOAD = "{{<img src=x onerror=alert(1)>}}";

function mount(value: string) {
  const onChange = vi.fn<(next: string) => void>();
  const view = render(<RichTextEditor label="Text" value={value} onChange={onChange} readOnly={false} onTarget={() => undefined} />);
  return { onChange, editor: screen.getByRole("textbox", { name: "Text" }), ...view };
}

function clipboard(data: Record<string, string>) {
  return { clipboardData: { getData: (type: string) => data[type] ?? "" } };
}

describe("a merge tag with markup in it", () => {
  it("is text in the editor, not an element, and goes back out as written", () => {
    const { editor, onChange } = mount(`<p>${PAYLOAD}</p>`);

    expect(editor.querySelector("img")).toBeNull();
    expect(editor.textContent).toBe(PAYLOAD);
    fireEvent.input(editor);
    expect(onChange).toHaveBeenLastCalledWith(`<p>${PAYLOAD}</p>`);
  });

  it("stays text when the editor is shown again for another value and back", () => {
    const { editor, rerender } = mount("<p>one</p>");
    const again = (value: string) => rerender(<RichTextEditor label="Text" value={value} onChange={() => undefined} readOnly={false} onTarget={() => undefined} />);

    again(`<p>${PAYLOAD}</p>`);
    again("<p>two</p>");
    again(`<p>${PAYLOAD}</p>`);

    expect(editor.querySelector("img")).toBeNull();
    expect(editor.querySelector("*:not(p)")).toBeNull();
  });

  it("is escaped for the DOM and as written for the stored value", () => {
    expect(richTextToEditor(`<p>${PAYLOAD}</p>`)).toBe("<p>{{&lt;img src=x onerror=alert(1)&gt;}}</p>");
    expect(cleanRichText(`<p>${PAYLOAD}</p>`)).toBe(`<p>${PAYLOAD}</p>`);
    expect(richTextToEditor('<p><a href="{{ u }}">x</a></p>')).toBe('<p><a href="{{ u }}">x</a></p>');
  });

  it("keeps a comparison in a merge tag a comparison", () => {
    const { editor, onChange } = mount("<p>{{ a > 1 }}</p>");
    fireEvent.input(editor);
    expect(onChange).toHaveBeenLastCalledWith("<p>{{ a > 1 }}</p>");
  });
});

describe("paste and drop", () => {
  it("cleans pasted HTML before it reaches the DOM", () => {
    const { editor, onChange } = mount("");

    const accepted = fireEvent.paste(editor, clipboard({ "text/html": '<p onclick="x()">hi <b>there</b></p><img src="https://remote.example/p.png"><script>bad()</script>' }));

    expect(accepted).toBe(false);
    expect(editor.querySelector("img, script")).toBeNull();
    expect(editor.querySelector("[onclick]")).toBeNull();
    expect(editor.querySelector("p b")?.textContent).toBe("there");
    expect(onChange).toHaveBeenLastCalledWith("<p>hi <b>there</b></p>");
  });

  it("pastes plain text as text, with line breaks, never as markup", () => {
    const { editor, onChange } = mount("");

    fireEvent.paste(editor, clipboard({ "text/plain": "a <img src=x>\nb" }));

    expect(editor.querySelector("img")).toBeNull();
    expect(editor.querySelectorAll("br")).toHaveLength(1);
    expect(onChange).toHaveBeenLastCalledWith("<p>a &lt;img src=x&gt;<br>b</p>");
  });

  it("cleans dropped HTML too", () => {
    const { editor, onChange } = mount("");

    const accepted = fireEvent.drop(editor, { dataTransfer: { getData: (type: string) => (type === "text/html" ? '<img src="https://remote.example/p.png"><i>kept</i>' : "") } });

    expect(accepted).toBe(false);
    expect(editor.querySelector("img")).toBeNull();
    expect(onChange).toHaveBeenLastCalledWith("<p><i>kept</i></p>");
  });
});

describe("link addresses", () => {
  it("accept only what the server accepts: a merge tag must start with {{", () => {
    expect(linkAllowed("{{ system.unsubscribe_url }}")).toBe(true);
    expect(linkAllowed("{% if x %}https://a.example{% endif %}")).toBe(false);
    expect(cleanRichText('<p><a href="{% if x %}https://a.example{% endif %}">go</a></p>')).toBe("<p>go</p>");
  });
});

describe("the UTM opt-out of a link", () => {
  it("is the one data attribute the clean pass keeps, and only as off", () => {
    expect(cleanRichText('<p><a href="https://a.example" data-zf-utm="off">go</a></p>')).toBe('<p><a href="https://a.example" data-zf-utm="off">go</a></p>');
    expect(cleanRichText('<p><a href="https://a.example" data-zf-utm="OFF">go</a></p>')).toBe('<p><a href="https://a.example" data-zf-utm="off">go</a></p>');
    expect(cleanRichText('<p><a href="https://a.example" data-zf-utm="on" data-zf-x="1" data-other="off">go</a></p>')).toBe('<p><a href="https://a.example">go</a></p>');
    expect(cleanRichText('<p><b data-zf-utm="off">go</b></p>')).toBe("<p><b>go</b></p>");
  });

  it("is set by the link dialog checkbox and cleared without it", () => {
    const link = vi.fn((_command: string, _ui: boolean, address?: string) => {
      const selection = window.getSelection();
      const root = document.querySelector<HTMLElement>("[role=textbox]");
      const text = root?.querySelector("p")?.firstChild;
      if (!selection || !root || !text) return false;
      let anchor = root.querySelector("a");
      if (!anchor) {
        anchor = document.createElement("a");
        anchor.textContent = text.textContent;
        text.parentNode?.replaceChild(anchor, text);
      }
      anchor.setAttribute("href", address ?? "");
      const range = document.createRange();
      range.selectNodeContents(anchor);
      selection.removeAllRanges();
      selection.addRange(range);
      return true;
    });
    Object.defineProperty(document, "execCommand", { value: link, configurable: true });
    try {
      const { onChange, editor } = mount("plain");
      const open = (check: boolean) => {
        const range = document.createRange();
        range.selectNodeContents(editor);
        window.getSelection()?.removeAllRanges();
        window.getSelection()?.addRange(range);
        fireEvent.mouseUp(editor);
        fireEvent.mouseDown(screen.getByRole("button", { name: "Add a link" }));
        fireEvent.click(screen.getByRole("button", { name: "Add a link" }));
        const box = screen.getByRole("checkbox", { name: "Do not add UTM parameters" });
        if (check) fireEvent.click(box);
        fireEvent.click(screen.getByRole("button", { name: "Apply link" }));
      };

      open(true);
      expect(onChange).toHaveBeenLastCalledWith('<p><a href="https://" data-zf-utm="off">plain</a></p>');

      open(false);
      expect(onChange).toHaveBeenLastCalledWith('<p><a href="https://">plain</a></p>');
    } finally {
      Reflect.deleteProperty(document, "execCommand");
    }
  });
});
