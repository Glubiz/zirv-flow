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
