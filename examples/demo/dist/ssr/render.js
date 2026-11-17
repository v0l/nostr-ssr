const escape = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

globalThis.ssrRender = (json) => {
  const { url } = JSON.parse(json);
  if (url !== "/") {
    return JSON.stringify({ html: "<h1>Not found</h1>", status: 404, head: { title: "Not found" }, data: {} });
  }
  const notes = JSON.parse(relayQuery(JSON.stringify([{ kinds: [1], limit: 10 }])));
  const items = notes.map((n) => `<li>${escape(n.content.slice(0, 140))}</li>`).join("");
  return JSON.stringify({
    html: `<h1>Latest notes</h1><ul>${items}</ul>`,
    status: 200,
    head: { title: "Latest notes", description: "Ten recent notes from a relay", path: "/" },
    data: { count: notes.length },
  });
};
