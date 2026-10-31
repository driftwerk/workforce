// The wf-form renderer: a validated FormSpec becomes whitelisted controls (one
// per field type), client-validated for UX only. On submit it builds the human
// summary + the exact machine values and hands them to the running conversation
// through the host's `submitForm`, then locks — the agent's next turn (a
// confirmation or a re-emitted form) supersedes it. A host that cannot take a
// submission gets a read-only form. Large/dynamic option sets resolve through a
// typeahead over the host's staged-table pages.

import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";

import {
  buildSummary,
  buildValues,
  fieldError,
  formValid,
  initialFormState,
  optionsFromPage,
  type FieldState,
  type FormField,
  type FormOption,
  type FormSpec,
} from "./spec";
import { TableExpired, useHost } from "./host";

const OPTIONS_LIMIT = 20;
const FETCH_DEBOUNCE_MS = 250;

/** A typeahead over a staged tabular artifact. Picking a returned row commits
 *  its `value`; if the artifact has expired, or the host cannot page it, it
 *  degrades to a free-text input so the form stays submittable. */
function OptionsRefField({
  id,
  field,
  value,
  invalid,
  disabled,
  onPick,
}: {
  id: string;
  field: FormField;
  value: string;
  invalid: boolean;
  disabled: boolean;
  onPick: (value: string, label: string) => void;
}) {
  const { latest, tables } = useHost();
  const ref = field.optionsRef!;
  const [text, setText] = useState(value);
  const [options, setOptions] = useState<FormOption[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [expired, setExpired] = useState(false);
  const [open, setOpen] = useState(false);
  const seq = useRef(0);
  const freeText = expired || !tables;

  useEffect(() => {
    const host = latest.current;
    const fetchPage = host.fetchTablePage?.bind(host);
    if (freeText || !fetchPage) return;
    const mine = ++seq.current;
    setStatus("loading");
    const handle = setTimeout(() => {
      fetchPage(ref.artifactId, {
        offset: 0,
        limit: OPTIONS_LIMIT,
        search: text.trim() || undefined,
      })
        .then((page) => {
          if (mine !== seq.current) return;
          setOptions(optionsFromPage(page, ref));
          setStatus("ready");
        })
        .catch((e) => {
          if (mine !== seq.current) return;
          if (e instanceof TableExpired) setExpired(true);
          else setStatus("error");
        });
    }, FETCH_DEBOUNCE_MS);
    return () => clearTimeout(handle);
  }, [latest, ref, text, freeText]);

  if (freeText) {
    return (
      <input
        id={id}
        className="wf-form-input"
        type="text"
        value={text}
        disabled={disabled}
        aria-invalid={invalid || undefined}
        placeholder="Options unavailable — type a value"
        onChange={(e) => {
          setText(e.target.value);
          onPick(e.target.value, e.target.value);
        }}
      />
    );
  }
  return (
    <div className="wf-form-combo">
      <input
        id={id}
        className="wf-form-input"
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-invalid={invalid || undefined}
        autoComplete="off"
        value={text}
        disabled={disabled}
        placeholder={field.placeholder ?? "Search…"}
        onChange={(e) => {
          setText(e.target.value);
          onPick("", "");
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setTimeout(() => setOpen(false), 120)}
      />
      {open && (
        <ul className="wf-form-options" role="listbox">
          {status === "loading" && <li className="wf-form-note wf-form-opt-msg">Loading…</li>}
          {status === "error" && <li className="wf-form-note wf-form-opt-msg">Couldn&apos;t load choices</li>}
          {status === "ready" && options.length === 0 && (
            <li className="wf-form-note wf-form-opt-msg">No matches</li>
          )}
          {options.map((o) => (
            <li key={o.value}>
              <button
                type="button"
                className="wf-form-option"
                // mousedown fires before the input's blur, so the pick lands.
                onMouseDown={(e) => {
                  e.preventDefault();
                  setText(o.label);
                  onPick(o.value, o.label);
                  setOpen(false);
                }}
              >
                {o.label}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function CheckGroup({
  field,
  value,
  disabled,
  onChange,
}: {
  field: FormField;
  value: string[];
  disabled: boolean;
  onChange: (value: string[]) => void;
}) {
  const chosen = new Set(value);
  const toggle = (v: string, on: boolean) => {
    if (on) chosen.add(v);
    else chosen.delete(v);
    onChange(field.options!.filter((o) => chosen.has(o.value)).map((o) => o.value));
  };
  return (
    <div className="wf-form-group" role="group">
      {field.options!.map((o) => (
        <label key={o.value} className="wf-form-check">
          <input
            type="checkbox"
            disabled={disabled}
            checked={chosen.has(o.value)}
            onChange={(e) => toggle(o.value, e.target.checked)}
          />
          {o.label}
        </label>
      ))}
    </div>
  );
}

function RadioGroup({
  field,
  value,
  disabled,
  onChange,
}: {
  field: FormField;
  value: string;
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  return (
    <div className="wf-form-group" role="radiogroup">
      {field.options!.map((o) => (
        <label key={o.value} className="wf-form-check">
          <input
            type="radio"
            name={field.name}
            disabled={disabled}
            checked={value === o.value}
            onChange={() => onChange(o.value)}
          />
          {o.label}
        </label>
      ))}
    </div>
  );
}

function Control({
  id,
  field,
  value,
  invalid,
  disabled,
  onChange,
  onPick,
}: {
  id: string;
  field: FormField;
  value: FieldState;
  invalid: boolean;
  disabled: boolean;
  onChange: (value: FieldState) => void;
  onPick: (value: string, label: string) => void;
}): ReactNode {
  const common = {
    id,
    disabled,
    className: "wf-form-input",
    "aria-invalid": invalid || undefined,
  };
  switch (field.type) {
    case "text":
    case "email":
      return (
        <input
          {...common}
          type={field.type}
          value={value as string}
          placeholder={field.placeholder}
          maxLength={field.maxLength}
          onChange={(e) => onChange(e.target.value)}
        />
      );
    case "textarea":
      return (
        <textarea
          {...common}
          rows={field.rows ?? 3}
          value={value as string}
          placeholder={field.placeholder}
          maxLength={field.maxLength}
          onChange={(e) => onChange(e.target.value)}
        />
      );
    case "number":
      return (
        <input
          {...common}
          type="number"
          value={value as string}
          placeholder={field.placeholder}
          min={field.min}
          max={field.max}
          step={field.step}
          onChange={(e) => onChange(e.target.value)}
        />
      );
    case "date":
      return (
        <input
          {...common}
          type="date"
          value={value as string}
          min={field.dateMin}
          max={field.dateMax}
          onChange={(e) => onChange(e.target.value)}
        />
      );
    case "select":
      if (field.optionsRef) {
        return (
          <OptionsRefField
            id={id}
            field={field}
            value={value as string}
            invalid={invalid}
            disabled={disabled}
            onPick={onPick}
          />
        );
      }
      if (field.multiple) {
        return (
          <select
            {...common}
            className="wf-form-input wf-form-select"
            multiple
            value={value as string[]}
            onChange={(e) => onChange(Array.from(e.target.selectedOptions, (o) => o.value))}
          >
            {field.options!.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
        );
      }
      return (
        <select
          {...common}
          className="wf-form-input wf-form-select"
          value={value as string}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">{field.placeholder ?? "Select…"}</option>
          {field.options!.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      );
    case "checkbox":
      if (field.options) {
        return (
          <CheckGroup
            field={field}
            value={value as string[]}
            disabled={disabled}
            onChange={onChange}
          />
        );
      }
      return (
        <label className="wf-form-check">
          <input
            id={id}
            type="checkbox"
            disabled={disabled}
            checked={value === true}
            onChange={(e) => onChange(e.target.checked)}
          />
          {field.label}
          {field.required && (
            <span className="wf-form-req" aria-hidden="true">
              {" *"}
            </span>
          )}
        </label>
      );
    case "radio":
      return (
        <RadioGroup field={field} value={value as string} disabled={disabled} onChange={onChange} />
      );
    case "hidden":
      return null;
  }
}

function FieldRow({
  field,
  value,
  showErrors,
  disabled,
  onChange,
  onPick,
}: {
  field: FormField;
  value: FieldState;
  showErrors: boolean;
  disabled: boolean;
  onChange: (value: FieldState) => void;
  onPick: (value: string, label: string) => void;
}) {
  const error = (showErrors ? fieldError(field, value) : null) ?? field.error ?? null;
  const invalid = !!error;
  const id = "wf-field-" + field.name;
  // Radio/checkbox groups label a group (not one input); a single checkbox carries
  // its own inline label, so no separate field label is drawn above it.
  const isGroup = field.type === "radio" || (field.type === "checkbox" && !!field.options);
  const isSingleCheck = field.type === "checkbox" && !field.options;
  const label = (
    <>
      {field.label}
      {field.required && (
        <span className="wf-form-req" aria-hidden="true">
          {" *"}
        </span>
      )}
    </>
  );
  return (
    <div className="wf-form-field">
      {isSingleCheck ? null : isGroup ? (
        <span className="wf-form-label">{label}</span>
      ) : (
        <label className="wf-form-label" htmlFor={id}>
          {label}
        </label>
      )}
      <Control
        id={id}
        field={field}
        value={value}
        invalid={invalid}
        disabled={disabled}
        onChange={onChange}
        onPick={onPick}
      />
      {field.help && <div className="wf-form-help">{field.help}</div>}
      {error && (
        <div className="wf-form-error" role="alert">
          {error}
        </div>
      )}
    </div>
  );
}

export default function WfForm({ spec }: { spec: FormSpec }): ReactNode {
  const { latest, forms, answered } = useHost();
  const [state, setState] = useState<Record<string, FieldState>>(() => initialFormState(spec));
  // Labels resolved by an options_ref typeahead, so the summary reads the chosen
  // row's label rather than its raw id.
  const [resolved, setResolved] = useState<Record<string, Record<string, string>>>({});
  const [showErrors, setShowErrors] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const done = submitted || answered;
  const locked = done || !forms;

  const setField = (name: string, value: FieldState) =>
    setState((s) => ({ ...s, [name]: value }));
  const pickRef = (name: string, value: string, label: string) => {
    setState((s) => ({ ...s, [name]: value }));
    setResolved((r) => ({ ...r, [name]: value ? { [value]: label } : {} }));
  };

  const valid = formValid(spec, state);
  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    const host = latest.current;
    if (done || !host.submitForm) return;
    setShowErrors(true);
    if (!formValid(spec, state)) return;
    setSubmitted(true);
    const delivered = await host.submitForm(
      buildSummary(spec, state, resolved),
      buildValues(spec, state),
    );
    if (!delivered) setSubmitted(false);
  };

  return (
    <form className="wf-form" onSubmit={onSubmit}>
      {spec.title && <div className="wf-form-title">{spec.title}</div>}
      {spec.note && <div className="wf-form-note">{spec.note}</div>}
      {spec.fields.map((field) =>
        field.type === "hidden" ? null : (
          <FieldRow
            key={field.name}
            field={field}
            value={state[field.name]}
            showErrors={showErrors}
            disabled={locked}
            onChange={(v) => setField(field.name, v)}
            onPick={(v, label) => pickRef(field.name, v, label)}
          />
        ),
      )}
      <div className="wf-form-actions">
        <button
          type="submit"
          className="wf-form-submit"
          disabled={locked || (showErrors && !valid)}
        >
          {done ? "Submitted" : spec.submitLabel}
        </button>
      </div>
    </form>
  );
}
