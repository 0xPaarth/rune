// Injected via chrome.scripting.executeScript. The value of the final
// expression (the IIFE result) is returned to popup.js as the injection result.
(() => {
  /**
   * Codeforces renders sample test content inside <pre> tags where
   * newlines are <br> elements and some spaces are &nbsp; entities.
   * `textContent` alone flattens <br> away, producing single-line garbage.
   *
   * This walks child nodes recursively:
   *   - <br>          -> '\n'
   *   - text node     -> its text, with non-breaking spaces ( )
   *                      normalized to regular spaces
   *   - <div> wrapper -> recurse, then ensure a trailing '\n' (newer CF
   *                      pages wrap each input line in
   *                      <div class="test-example-line">)
   */
  function extractPreText(node) {
    let out = '';
    for (const child of node.childNodes) {
      if (child.nodeType === Node.TEXT_NODE) {
        out += child.textContent.replace(/ /g, ' ');
      } else if (child.nodeType === Node.ELEMENT_NODE) {
        if (child.tagName === 'BR') {
          out += '\n';
        } else {
          out += extractPreText(child);
          // Block-level line wrappers imply a line break even without <br>
          if (child.tagName === 'DIV' && !out.endsWith('\n')) {
            out += '\n';
          }
        }
      }
    }
    return out;
  }

  // Normalize: strip trailing whitespace per line, ensure single trailing \n
  function cleanIoText(raw) {
    return raw
      .replace(/\r\n/g, '\n')
      .split('\n')
      .map((line) => line.replace(/\s+$/, ''))
      .join('\n')
      .replace(/\n+$/, '') + '\n';
  }

  try {
    // --- contestId / index / url from the pathname ---
    // /contest/1922/problem/C        -> contestId=1922, index=C
    // /problemset/problem/1922/C     -> contestId=1922, index=C
    const path = window.location.pathname;
    let match =
      path.match(/^\/contest\/(\d+)\/problem\/([A-Za-z0-9]+)/) ||
      path.match(/^\/problemset\/problem\/(\d+)\/([A-Za-z0-9]+)/);

    if (!match) {
      return { error: `Unrecognized problem URL path: ${path}` };
    }

    const contestId = parseInt(match[1], 10);
    const index = match[2];
    const url = `https://codeforces.com/contest/${contestId}/problem/${index}`;

    const statement = document.querySelector('.problem-statement');
    if (!statement) {
      return { error: 'No .problem-statement found on page.' };
    }

    // --- name: ".title" is "C. Closest to the Right" -> strip "C. " prefix ---
    const titleEl = statement.querySelector('.title');
    const rawTitle = titleEl ? titleEl.textContent.trim() : '';
    const name = rawTitle.replace(/^[A-Za-z0-9]+\s*\.\s*/, '');

    // --- rating & tags from the sidebar tag boxes ---
    // Rating lives in a tag-box that looks like "*1600".
    let rating;
    const tags = [];
    for (const tagEl of document.querySelectorAll('.tag-box')) {
      const text = tagEl.textContent.trim();
      const ratingMatch = text.match(/^\*(\d+)$/);
      if (ratingMatch) {
        rating = parseInt(ratingMatch[1], 10);
      } else {
        tags.push(text);
      }
    }

    // --- time / memory limits ---
    // .time-limit contains a hidden label div + "2 seconds" text.
    const timeLimitEl = statement.querySelector('.time-limit');
    const timeMatch = timeLimitEl
      ? timeLimitEl.textContent.match(/([\d.]+)\s*second/)
      : null;
    const timeLimitMs = timeMatch ? Math.round(parseFloat(timeMatch[1]) * 1000) : 0;

    const memLimitEl = statement.querySelector('.memory-limit');
    const memMatch = memLimitEl
      ? memLimitEl.textContent.match(/(\d+)\s*megabyte/)
      : null;
    const memoryLimitMb = memMatch ? parseInt(memMatch[1], 10) : 0;

    // --- test cases ---
    // .sample-test contains alternating .input / .output divs, each with a <pre>.
    const testCases = [];
    const sampleTest = statement.querySelector('.sample-test');
    if (sampleTest) {
      const inputs = sampleTest.querySelectorAll('.input pre');
      const outputs = sampleTest.querySelectorAll('.output pre');
      const count = Math.min(inputs.length, outputs.length);

      for (let i = 0; i < count; i++) {
        testCases.push({
          id: `sample-${i + 1}`,
          input: cleanIoText(extractPreText(inputs[i])),
          expectedOutput: cleanIoText(extractPreText(outputs[i])),
        });
      }
    }

    if (testCases.length === 0) {
      return { error: 'No sample test cases found.' };
    }

    // --- descriptionHtml ---
    // Clone .problem-statement and strip the chunks we already capture
    // structurally (header block with title/limits, plus the sample-tests
    // block). Whatever remains is the actual prose: statement, input spec,
    // output spec, notes — which Turndown will convert on the daemon side.
    const statementClone = statement.cloneNode(true);
    statementClone.querySelectorAll('.header, .sample-tests').forEach((n) => n.remove());
    const descriptionHtml = statementClone.innerHTML.trim();

    const payload = {
      contestId,
      index,
      name,
      url,
      tags,
      timeLimitMs,
      memoryLimitMb,
      descriptionHtml,
      testCases,
    };
    if (rating !== undefined) payload.rating = rating;

    return payload;
  } catch (err) {
    return { error: `Scraper exception: ${err.message}` };
  }
})();
