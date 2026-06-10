/**
 * Tiny vanilla-DOM form primitives shared by the settings panel and any
 * future panel that surfaces user preferences. Each helper returns a
 * `{ element, setValue }` handle so callers can react to external
 * preference changes without rebuilding the DOM.
 */

export type FormRow<T> = {
  element: HTMLElement;
  setValue: (value: T) => void;
  setDisabled?: (disabled: boolean) => void;
};

export type SliderRowOptions = {
  id: string;
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  format?: (value: number) => string;
  onInput: (value: number) => void;
};

export type SelectOption<T extends string> = {
  value: T;
  label: string;
};

export type SelectRowOptions<T extends string> = {
  id: string;
  label: string;
  value: T;
  options: readonly SelectOption<T>[];
  onChange: (value: T) => void;
};

export type ToggleRowOptions = {
  id: string;
  label: string;
  value: boolean;
  disabled?: boolean;
  description?: string;
  onChange: (value: boolean) => void;
};

export function createSectionHeader(label: string, hint?: string): HTMLElement {
  const header = document.createElement('div');
  header.className = 'ui-form__section-header';
  header.dataset.testid = `ui-form-section-${slug(label)}`;

  const title = document.createElement('h3');
  title.className = 'ui-form__section-title';
  title.textContent = label;
  header.append(title);

  if (hint) {
    const hintEl = document.createElement('p');
    hintEl.className = 'ui-form__section-hint';
    hintEl.textContent = hint;
    header.append(hintEl);
  }
  return header;
}

export function createSliderRow(options: SliderRowOptions): FormRow<number> {
  const row = createRow(options.id);
  const label = document.createElement('label');
  label.className = 'ui-form__label';
  label.htmlFor = options.id;
  label.textContent = options.label;

  const valueLabel = document.createElement('span');
  valueLabel.className = 'ui-form__value';
  valueLabel.dataset.testid = `${options.id}-value`;

  const input = document.createElement('input');
  input.type = 'range';
  input.id = options.id;
  input.className = 'ui-form__slider';
  input.min = String(options.min);
  input.max = String(options.max);
  input.step = String(options.step);
  input.value = clampToStep(options.value, options).toString();

  const formatValue = (value: number): string =>
    options.format ? options.format(value) : value.toString();
  valueLabel.textContent = formatValue(Number(input.value));

  input.addEventListener('input', () => {
    const numeric = clampToStep(Number(input.value), options);
    valueLabel.textContent = formatValue(numeric);
    options.onInput(numeric);
  });

  row.append(label, valueLabel, input);
  return {
    element: row,
    setValue(value) {
      const numeric = clampToStep(value, options);
      input.value = numeric.toString();
      valueLabel.textContent = formatValue(numeric);
    },
  };
}

export function createSelectRow<T extends string>(options: SelectRowOptions<T>): FormRow<T> {
  const row = createRow(options.id);
  const label = document.createElement('label');
  label.className = 'ui-form__label';
  label.htmlFor = options.id;
  label.textContent = options.label;

  const select = document.createElement('select');
  select.id = options.id;
  select.className = 'ui-form__select';
  for (const choice of options.options) {
    const opt = document.createElement('option');
    opt.value = choice.value;
    opt.textContent = choice.label;
    select.append(opt);
  }
  select.value = options.value;
  select.addEventListener('change', () => {
    options.onChange(select.value as T);
  });

  row.append(label, select);
  return {
    element: row,
    setValue(value) {
      select.value = value;
    },
  };
}

export function createToggleRow(options: ToggleRowOptions): FormRow<boolean> {
  const row = createRow(options.id);
  row.classList.add('ui-form__row--toggle');

  const text = document.createElement('div');
  text.className = 'ui-form__toggle-text';
  const label = document.createElement('label');
  label.className = 'ui-form__label';
  label.htmlFor = options.id;
  label.textContent = options.label;
  text.append(label);
  if (options.description) {
    const desc = document.createElement('p');
    desc.className = 'ui-form__description';
    desc.textContent = options.description;
    text.append(desc);
  }

  const input = document.createElement('input');
  input.type = 'checkbox';
  input.id = options.id;
  input.className = 'ui-form__toggle';
  input.checked = options.value;
  if (options.disabled) {
    input.disabled = true;
  }
  input.addEventListener('change', () => {
    options.onChange(input.checked);
  });

  row.append(text, input);
  return {
    element: row,
    setValue(value) {
      input.checked = value;
    },
    setDisabled(disabled) {
      input.disabled = disabled;
    },
  };
}

function createRow(id: string): HTMLElement {
  const row = document.createElement('div');
  row.className = 'ui-form__row';
  row.dataset.testid = `ui-form-row-${id}`;
  return row;
}

function clampToStep(
  value: number,
  options: { min: number; max: number; step: number },
): number {
  if (!Number.isFinite(value)) {
    return options.min;
  }
  const clamped = Math.min(options.max, Math.max(options.min, value));
  if (!Number.isFinite(options.step) || options.step <= 0) {
    return clamped;
  }
  const steps = Math.round((clamped - options.min) / options.step);
  const snapped = options.min + steps * options.step;
  // Round to suppress floating-point noise so test assertions stay clean.
  const decimals = decimalsFor(options.step);
  return Number(snapped.toFixed(decimals));
}

function decimalsFor(step: number): number {
  if (step >= 1) {
    return 0;
  }
  const text = step.toString();
  const dot = text.indexOf('.');
  return dot === -1 ? 0 : text.length - dot - 1;
}

function slug(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/(^-|-$)/g, '');
}
