// Number presentation for rich blocks. Values stay numeric everywhere else —
// sorting, range filters, CSV export, chart data — and only the text a person
// reads passes through here, so a formatted table still sorts by value.
//
// The agent may declare a column's `format`; otherwise the label and the values
// decide. The two mistakes worth avoiding are grouping an identifier
// ("1,024,773" for a reference) and grouping a year ("2,026"), so those
// classify as `id` and render untouched.

export type ColumnFormat = "number" | "currency" | "integer" | "percent" | "id" | "text";

export const COLUMN_FORMATS: readonly ColumnFormat[] = [
  "number",
  "currency",
  "integer",
  "percent",
  "id",
  "text",
];

export interface NumberStyle {
  format?: ColumnFormat;
  /** A prefix such as `£`; `%` is treated as a suffix. */
  unit?: string;
}

// One fixed locale rather than the browser's: an axis tick from ECharts and a
// tooltip from Intl must agree on the separators, and the data is read the same
// way by everyone who opens the block.
const LOCALE = "en-GB";
const fraction = (min: number, max: number) =>
  new Intl.NumberFormat(LOCALE, { minimumFractionDigits: min, maximumFractionDigits: max });
const NUMBER = fraction(0, 2);
const INTEGER = fraction(0, 0);
const CURRENCY = fraction(2, 2);
const PERCENT = fraction(0, 1);

function withUnit(digits: string, negative: boolean, unit: string): string {
  const sign = negative ? "-" : "";
  return unit === "%" ? `${sign}${digits}%` : `${sign}${unit}${digits}`;
}

/** The display text for a number: thousands separators and sensible decimals
 *  (whole numbers get none, fractions at most two, money exactly two). */
export function formatNumber(value: number, style: NumberStyle = {}): string {
  if (!Number.isFinite(value)) return String(value);
  const unit = style.unit ?? "";
  const magnitude = Math.abs(value);
  const negative = value < 0;
  switch (style.format) {
    case "id":
    case "text":
      return String(value);
    case "integer":
      return withUnit(INTEGER.format(magnitude), negative, unit);
    case "currency":
      return withUnit(CURRENCY.format(magnitude), negative, unit);
    case "percent":
      return withUnit(PERCENT.format(magnitude), negative, "%");
    default:
      return withUnit(NUMBER.format(magnitude), negative, unit);
  }
}

/** "Number of leads" is a count; "Quote number" is an identifier. */
const COUNT_LABEL = /^(number|no\.?|count|total)\s+of\b/i;
const ID_LABEL =
  /\b(id|ids|ref|reference|number|no|year|yr|postcode|zip|phone|tel|mobile|code|uprn|sku|account)\b/i;
const YEAR_MIN = 1900;
const YEAR_MAX = 2100;

/** The format for a numeric column the agent did not annotate: identifiers and
 *  years render untouched, everything else is a measure. */
export function autoFormat(label: string, values: readonly number[]): ColumnFormat {
  // Server columns arrive as snake_case keys ("lead_id"); read them as words.
  const words = label.replace(/[_-]+/g, " ");
  if (COUNT_LABEL.test(words)) return "number";
  if (ID_LABEL.test(words)) return "id";
  if (
    values.length > 0 &&
    values.every((v) => Number.isInteger(v) && v >= YEAR_MIN && v <= YEAR_MAX)
  ) {
    return "id";
  }
  return "number";
}

/** True when every value reads as a calendar year — used to keep a scatter's
 *  year axis from being grouped as "2,026". */
export function looksLikeYears(values: readonly number[]): boolean {
  return values.length > 0 && autoFormat("", values) === "id";
}
