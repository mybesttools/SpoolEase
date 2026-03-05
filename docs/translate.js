/**
 * SpoolEase Inventory Translator
 * Hosted on GitHub Pages: https://mybesttools.github.io/SpoolEase/translate.js
 *
 * Applies UI translations to the inventory editor via MutationObserver.
 * Fetches the active language from the device's /api/locale endpoint.
 */

(async function () {
  // ── Language detection ───────────────────────────────────────────────────
  let lang = 'en';
  try {
    const res = await fetch('/api/locale', { cache: 'no-store' });
    const data = await res.json();
    lang = (data.language || 'en').toLowerCase();
  } catch (_) {}

  if (lang === 'en') return; // nothing to do

  // ── Translation tables ────────────────────────────────────────────────────
  const TRANSLATIONS = {
    pl: {
      // ── Navigation / top bar ─────────────────────────────────────────────
      'Inventory': 'Inwentarz',

      // ── Filter panel ─────────────────────────────────────────────────────
      'Filters': 'Filtry',
      'Clear Filters': 'Wyczyść filtry',
      'Clear': 'Wyczyść',
      '(Enter keywords (comma-separated, may include spaces)': '(Wpisz słowa kluczowe rozdzielone przecinkami)',
      '(Comma-seperated keywords)': '(Słowa kluczowe rozdzielone przecinkami)',

      'Material:': 'Materiał:',
      'Subtype:': 'Podtyp:',
      'Color Name:': 'Nazwa koloru:',
      'Brand:': 'Marka:',
      'Note:': 'Notatka:',
      'Color:': 'Kolor:',
      'Location:': 'Lokalizacja:',
      'Unused:': 'Nieużywana:',
      'Weight:': 'Waga:',
      'Label:': 'Etykieta:',
      'Min:': 'Min:',
      'Max:': 'Max:',
      'All': 'Wszystkie',
      'Unused': 'Nieużywana',
      'Used': 'Używana',
      'Unspecified': 'Nieokreślona',

      // ── Table column headers ──────────────────────────────────────────────
      'ID': 'ID',
      'Added': 'Dodano',
      'Color': 'Kolor',
      'Brand': 'Marka',
      'Slicer Filament': 'Filament (slicer)',
      'Location': 'Lokalizacja',
      'Label': 'Etykieta',
      'Net': 'Netto',
      'Gross': 'Brutto',
      'Material': 'Materiał',
      'Subtype': 'Podtyp',
      'Note': 'Notatka',
      'Tag': 'Tag',
      'Actions': 'Akcje',
      'Columns': 'Kolumny',

      // ── Spool edit / add dialog ───────────────────────────────────────────
      'Add Spool': 'Dodaj szpulę',
      'Edit Spool': 'Edytuj szpulę',
      'Add Similar': 'Dodaj podobną',
      'Add New': 'Dodaj nową',
      'Save': 'Zapisz',
      'Cancel': 'Anuluj',
      'Close': 'Zamknij',
      'Refresh': 'Odśwież',
      'Add': 'Dodaj',

      'Color Name': 'Nazwa koloru',
      'Hex Color': 'Kolor hex',
      'Label Weight': 'Waga etykiety',
      'Core Weight': 'Waga rdzenia',
      'Current Weight': 'Aktualna waga',
      'Slicer Filament Code': 'Kod filamentu (slicer)',
      'Full Unused': 'W pełni nieużywana',

      // ── Delete dialog ─────────────────────────────────────────────────────
      'Delete Spool': 'Usuń szpulę',
      'Are you sure you want to delete': 'Czy na pewno chcesz usunąć',
      'spool': 'szpulę',
      'Yes, Delete': 'Tak, usuń',
      'NOOO !!!': 'Nie!!!',

      // ── Security key ─────────────────────────────────────────────────────
      'Security Key:': 'Klucz bezpieczeństwa:',
      'Enter Security Key': 'Wprowadź klucz bezpieczeństwa',

      // ── Validation messages ───────────────────────────────────────────────
      'Material is required': 'Materiał jest wymagany',
      'Label W. Required': 'Wymagana waga etykiety',
      'Invalid hex color format': 'Nieprawidłowy format koloru hex',

      // ── Status / toast ────────────────────────────────────────────────────
      '✅ Spool Added': '✅ Szpula dodana',
      '✅ Spool Updated': '✅ Szpula zaktualizowana',
      '❌ Failed to Add Spool': '❌ Nie udało się dodać szpuli',
      '❌ Failed to Update Spool': '❌ Nie udało się zaktualizować szpuli',
      '❌ Failed to Update Encode Information': '❌ Nie udało się zaktualizować informacji',

      // ── Pressure advance / k-info section ────────────────────────────────
      'Nozzle Type:': 'Typ dyszy:',
      'Standard': 'Standardowa',
      'High Flow': 'Przepływ zwiększony',
      'Not in printer': 'Nie w drukarce',
      'Add This Pressure Advance Setting?': 'Dodać to ustawienie Pressure Advance?',
      'Printer:': 'Drukarka:',
      'Extruder ID:': 'Ekstruder ID:',
      'Nozzle:': 'Dysza:',

      // ── Column config ─────────────────────────────────────────────────────
      'Column Configuration': 'Konfiguracja kolumn',
      'Reset to Default': 'Przywróć domyślne',
      'Apply': 'Zastosuj',
      'Discard': 'Odrzuć',

      // ── Misc ──────────────────────────────────────────────────────────────
      'e.g. PLA, PETG': 'np. PLA, PETG',
      'Plus, High Speed,': 'Plus, High Speed,',
      'Loading…': 'Ładowanie…',
      'No spools found.': 'Nie znaleziono szpul.',
    },
  };

  const table = TRANSLATIONS[lang];
  if (!table) return;

  // ── Translation engine ────────────────────────────────────────────────────

  /**
   * Translate a single text node.
   * We try exact-match first, then substring replacements for longer strings.
   */
  function translateTextNode(node) {
    const original = node.textContent;
    const trimmed = original.trim();
    if (!trimmed) return;

    // Exact match (fast path)
    if (table[trimmed] !== undefined) {
      node.textContent = original.replace(trimmed, table[trimmed]);
      return;
    }

    // Substring match for partial strings (e.g. hint text with leading/trailing chars)
    for (const [src, dst] of Object.entries(table)) {
      if (src.length > 3 && original.includes(src)) {
        node.textContent = original.split(src).join(dst);
        return; // apply first matching replacement only to avoid double-translate
      }
    }
  }

  /**
   * Walk the subtree and translate all text nodes.
   * Skips <script>, <style>, <textarea>, <input>.
   */
  function translateSubtree(root) {
    const walker = document.createTreeWalker(
      root,
      NodeFilter.SHOW_TEXT,
      {
        acceptNode(node) {
          const parent = node.parentElement;
          if (!parent) return NodeFilter.FILTER_REJECT;
          const tag = parent.tagName;
          if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'TEXTAREA' || tag === 'INPUT') {
            return NodeFilter.FILTER_REJECT;
          }
          return NodeFilter.FILTER_ACCEPT;
        },
      }
    );
    let n;
    while ((n = walker.nextNode())) {
      translateTextNode(n);
    }
  }

  // ── Wait for the React app to mount, then start observing ────────────────

  function start() {
    const app = document.getElementById('app');
    if (!app) {
      setTimeout(start, 100);
      return;
    }

    // Translate whatever is already rendered
    translateSubtree(app);

    // Watch for React re-renders
    let pending = false;
    const observer = new MutationObserver(() => {
      if (pending) return;
      pending = true;
      // Debounce slightly so we don't run mid-render
      requestAnimationFrame(() => {
        translateSubtree(app);
        pending = false;
      });
    });

    observer.observe(app, { childList: true, subtree: true, characterData: false });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
