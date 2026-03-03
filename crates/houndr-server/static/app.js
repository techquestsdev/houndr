let timer = null;
let controller = null;
let lastResults = null;
const FILES_PER_BATCH = 30;
const MAX_FILE_DETAILS = 500;
const REPOS_PER_BATCH = 20;
let renderedRepoCount = 0;
const query = document.getElementById('query');
const files = document.getElementById('files');
const repos = document.getElementById('repos');
const caseI = document.getElementById('case-i');
const isRegex = document.getElementById('is-regex');
const status = document.getElementById('status');
const results = document.getElementById('results');

function debounce(fn, ms) {
  return function(...args) {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

function renderRepoHtml(ri) {
  const repo = lastResults[ri];
  const baseUrl = repoWebUrl(repo.url);
  const ref = repo.git_ref || 'main';

  const repoNameHtml = baseUrl
    ? '<a href="' + esc(baseUrl) + '" target="_blank" rel="noopener">' + esc(repo.repo) + '</a>'
    : esc(repo.repo);
  let html = '<div class="result-repo">' + repoNameHtml + '<span class="repo-toggles"><button class="repo-toggle" data-action="expand">expand all</button><button class="repo-toggle" data-action="collapse">collapse all</button></span></div>';
  html += '<div class="repo-files">';
  const batch = Math.min(FILES_PER_BATCH, repo.files.length);
  for (let fi = 0; fi < batch; fi++) {
    html += renderFileHtml(repo.files[fi], baseUrl, ref);
  }
  if (repo.files.length > FILES_PER_BATCH) {
    const remaining = repo.files.length - FILES_PER_BATCH;
    html += '<button class="show-more" data-repo="' + ri + '" data-offset="' + FILES_PER_BATCH + '">Show ' + remaining + ' more files\u2026</button>';
  }
  html += '</div>';
  return html;
}

const doSearch = debounce(async function() {
  const q = query.value.trim();
  if (q.length < 3) {
    results.innerHTML = '<div class="empty">Type at least 3 characters</div>';
    status.textContent = '';
    if (window.location.search) history.replaceState(null, '', window.location.pathname);
    return;
  }

  const params = new URLSearchParams({ q, max: MAX_FILE_DETAILS });
  if (files.value) params.set('files', files.value);
  if (repos.value) params.set('repos', repos.value);
  if (caseI.checked) params.set('i', 'true');
  if (isRegex.checked) params.set('regex', 'true');

  // Sync URL for shareability
  const urlParams = new URLSearchParams({ q });
  if (files.value) urlParams.set('files', files.value);
  if (repos.value) urlParams.set('repos', repos.value);
  if (caseI.checked) urlParams.set('i', 'true');
  if (isRegex.checked) urlParams.set('regex', 'true');
  history.replaceState(null, '', '?' + urlParams);

  // Cancel any in-flight request
  if (controller) controller.abort();
  controller = new AbortController();

  status.textContent = 'Searching...';
  results.innerHTML = '';
  lastResults = [];
  renderedRepoCount = 0;

  let totalFiles = 0;
  let totalMatches = 0;
  let loadedFiles = 0;
  let truncated = false;

  try {
    const res = await fetch('/api/v1/search/stream?' + params, { signal: controller.signal });

    if (!res.ok) {
      const data = await res.json();
      results.innerHTML = '<div class="empty">' + esc(data.error || 'Search failed') + '</div>';
      status.textContent = '';
      return;
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });

      // Parse SSE events from buffer
      let boundary;
      while ((boundary = buf.indexOf('\n\n')) !== -1) {
        const block = buf.slice(0, boundary);
        buf = buf.slice(boundary + 2);

        let eventType = 'message';
        let data = '';
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) eventType = line.slice(6).trim();
          else if (line.startsWith('data:')) data += line.slice(5);
        }
        if (!data) continue;

        if (eventType === 'result') {
          const repo = JSON.parse(data);
          const ri = lastResults.length;
          lastResults.push(repo);
          totalMatches += repo.total_match_count;
          loadedFiles += repo.files.length;
          if (repo.total_file_count > repo.files.length) truncated = true;
          if (ri < REPOS_PER_BATCH) {
            results.insertAdjacentHTML('beforeend', renderRepoHtml(ri));
            renderedRepoCount = ri + 1;
          } else {
            // Update or create "show more repos" button
            let moreBtn = results.querySelector('.show-more-repos');
            const pending = lastResults.length - renderedRepoCount;
            if (moreBtn) {
              moreBtn.textContent = 'Show ' + pending + ' more repos\u2026';
            } else {
              results.insertAdjacentHTML('beforeend', '<button class="show-more show-more-repos">Show ' + pending + ' more repos\u2026</button>');
            }
          }
          status.textContent = 'Searching\u2026 ' + lastResults.length + ' repo(s) found';
        } else if (eventType === 'done') {
          const summary = JSON.parse(data);
          totalFiles = summary.total_files;
          let statusText;
          if (truncated) {
            statusText = 'showing ' + loadedFiles + ' of ' + totalFiles + ' files, ' +
              totalMatches + ' matches in ' + summary.duration_ms.toFixed(1) + 'ms';
          } else {
            statusText = totalFiles + ' file(s), ' + totalMatches + ' match(es) in ' +
              summary.duration_ms.toFixed(1) + 'ms';
          }
          status.textContent = statusText;
        }
      }
    }

    if (lastResults.length === 0) {
      results.innerHTML = '<div class="empty">No results</div>';
      status.textContent = '';
    }
  } catch (e) {
    if (e.name === 'AbortError') return; // Request was cancelled by a newer search
    results.innerHTML = '<div class="empty">Error: ' + esc(e.message) + '</div>';
    status.textContent = '';
  }
}, 300);

// Event delegation for dynamically created elements
results.addEventListener('click', function(e) {
  // File header toggle
  var header = e.target.closest('.file-header');
  if (header && !e.target.closest('.file-link')) {
    header.parentElement.classList.toggle('collapsed');
    return;
  }
  // Expand/collapse all buttons
  var btn = e.target.closest('.repo-toggle');
  if (btn) {
    var group = btn.closest('.result-repo').nextElementSibling;
    if (btn.dataset.action === 'expand') {
      group.querySelectorAll('.result-file').forEach(function(f) { f.classList.remove('collapsed'); });
    } else if (btn.dataset.action === 'collapse') {
      group.querySelectorAll('.result-file').forEach(function(f) { f.classList.add('collapsed'); });
    }
    return;
  }
  // Show more repos button
  var showMoreRepos = e.target.closest('.show-more-repos');
  if (showMoreRepos && lastResults) {
    var end = Math.min(renderedRepoCount + REPOS_PER_BATCH, lastResults.length);
    var fragment = '';
    for (var ri = renderedRepoCount; ri < end; ri++) {
      fragment += renderRepoHtml(ri);
    }
    showMoreRepos.insertAdjacentHTML('beforebegin', fragment);
    renderedRepoCount = end;
    var remaining = lastResults.length - renderedRepoCount;
    if (remaining > 0) {
      showMoreRepos.textContent = 'Show ' + remaining + ' more repos\u2026';
    } else {
      showMoreRepos.remove();
    }
    return;
  }
  // Show more files button
  var showMore = e.target.closest('.show-more');
  if (showMore && lastResults) {
    var repoIdx = parseInt(showMore.dataset.repo);
    var offset = parseInt(showMore.dataset.offset);
    var repo = lastResults[repoIdx];
    var baseUrl = repoWebUrl(repo.url);
    var ref = repo.git_ref || 'main';
    var end = Math.min(offset + FILES_PER_BATCH, repo.files.length);
    var fragment = '';
    for (var fi = offset; fi < end; fi++) {
      fragment += renderFileHtml(repo.files[fi], baseUrl, ref);
    }
    showMore.insertAdjacentHTML('beforebegin', fragment);
    if (end < repo.files.length) {
      showMore.dataset.offset = end;
      showMore.textContent = 'Show ' + (repo.files.length - end) + ' more files\u2026';
    } else if (repo.total_file_count > repo.files.length) {
      showMore.replaceWith(Object.assign(document.createElement('div'), {
        className: 'truncation-note',
        textContent: 'showing ' + repo.files.length + ' of ' + repo.total_file_count +
          ' files \u2014 refine your search for more'
      }));
    } else {
      showMore.remove();
    }
  }
});

function repoWebUrl(url) {
  if (!url) return null;
  var clean = url.replace(/\.git$/, '');
  // SCP-style: git@host:org/repo → https://host/org/repo
  var scp = clean.match(/^[\w-]+@([^:]+):(.+)$/);
  if (scp) return 'https://' + scp[1] + '/' + scp[2];
  // ssh://git@host/org/repo → https://host/org/repo
  var ssh = clean.match(/^ssh:\/\/[\w-]+@([^/]+)\/(.+)$/);
  if (ssh) return 'https://' + ssh[1] + '/' + ssh[2];
  return clean;
}

function renderFileHtml(file, baseUrl, ref) {
  let html = '';
  const fileUrl = baseUrl
    ? baseUrl + '/blob/' + encodeURIComponent(ref) + '/' + file.path.split('/').map(encodeURIComponent).join('/')
    : null;
  const nameHtml = fileUrl
    ? '<a href="' + esc(fileUrl) + '" target="_blank" rel="noopener" class="file-link">' + esc(file.path) + '</a>'
    : esc(file.path);
  html += '<div class="result-file"><div class="file-header"><span class="collapse-icon">&#9660;</span>' + nameHtml + '</div>';
  html += '<div class="file-body">';
  for (let bi = 0; bi < file.blocks.length; bi++) {
    if (bi > 0) html += '<div class="block-sep">\u22EF</div>';
    for (const line of file.blocks[bi].lines) {
      const isCtx = !line.match_ranges || line.match_ranges.length === 0;
      const cls = isCtx ? 'line ctx' : 'line';
      const numHtml = fileUrl
        ? '<a href="' + esc(fileUrl) + '#L' + line.line_number + '" target="_blank" rel="noopener">' + line.line_number + '</a>'
        : '' + line.line_number;
      html += '<div class="' + cls + '"><span class="line-num">' + numHtml + '</span><span class="line-content">' + highlight(line.line, line.match_ranges) + '</span></div>';
    }
  }
  html += '</div></div>';
  return html;
}

function esc(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function highlight(line, ranges) {
  if (!ranges || ranges.length === 0) return esc(line);
  let result = '';
  let last = 0;
  for (const [start, end] of ranges) {
    result += esc(line.slice(last, start));
    result += '<mark>' + esc(line.slice(start, end)) + '</mark>';
    last = end;
  }
  result += esc(line.slice(last));
  return result;
}

document.querySelector('.top-bar h1').addEventListener('click', function() {
  query.value = '';
  files.value = '';
  repos.value = '';
  caseI.checked = false;
  isRegex.checked = false;
  results.innerHTML = '';
  status.textContent = '';
  lastResults = null;
  if (controller) { controller.abort(); controller = null; }
  clearTimeout(timer);
  history.replaceState(null, '', window.location.pathname);
  query.focus();
});

query.addEventListener('input', doSearch);
files.addEventListener('input', doSearch);
repos.addEventListener('input', doSearch);
caseI.addEventListener('change', doSearch);
isRegex.addEventListener('change', doSearch);

// Restore search from URL on page load
(function() {
  const p = new URLSearchParams(window.location.search);
  if (p.has('q')) {
    query.value = p.get('q');
    if (p.has('files')) files.value = p.get('files');
    if (p.has('repos')) repos.value = p.get('repos');
    if (p.get('i') === 'true') caseI.checked = true;
    if (p.get('regex') === 'true') isRegex.checked = true;
    doSearch();
  }
})();

// Indexing status polling
(function() {
  const indexStatus = document.getElementById('index-status');
  let pollTimer = null;
  let firstPoll = true;
  let everShown = false;

  async function pollStatus() {
    try {
      const res = await fetch('/api/v1/status');
      const data = await res.json();
      const entries = Object.entries(data);
      if (entries.length === 0) return;

      let ready = 0, indexing = 0, failed = 0;
      for (const [name, info] of entries) {
        if (info.status === 'ready') ready++;
        else if (info.status === 'indexing') indexing++;
        else if (info.status === 'failed') failed++;
      }
      const total = entries.length;
      const allDone = ready + failed >= total;

      // If everything is already ready on first poll, don't show anything
      if (firstPoll && allDone && failed === 0) {
        firstPoll = false;
        clearInterval(pollTimer);
        return;
      }
      firstPoll = false;

      let dotClass = 'ready';
      if (indexing > 0 || !allDone) dotClass = 'indexing';
      if (failed > 0 && indexing === 0 && allDone) dotClass = 'failed';

      let text = ready + '/' + total + ' repos ready';
      if (indexing > 0) text += ' \u00b7 ' + indexing + ' indexing';
      if (failed > 0) text += ' \u00b7 ' + failed + ' failed';

      indexStatus.innerHTML = '';
      const dot = document.createElement('span');
      dot.className = 'status-dot ' + dotClass;
      const span = document.createElement('span');
      span.className = 'status-text';
      span.textContent = text;
      if (failed > 0) {
        const failSpan = document.createElement('span');
        failSpan.className = 'fail-detail';
        failSpan.textContent = ' \u00b7 ' + failed + ' failed';
        span.textContent = ready + '/' + total + ' repos ready';
        if (indexing > 0) span.textContent += ' \u00b7 ' + indexing + ' indexing';
        span.appendChild(failSpan);
      }
      indexStatus.appendChild(dot);
      indexStatus.appendChild(span);
      indexStatus.classList.remove('hidden');
      everShown = true;

      if (allDone) {
        clearInterval(pollTimer);
        if (failed === 0) {
          setTimeout(function() { indexStatus.classList.add('hidden'); }, 3000);
        }
      }
    } catch (e) {
      // Silently ignore fetch errors
    }
  }

  pollStatus();
  pollTimer = setInterval(pollStatus, 3000);
})();
