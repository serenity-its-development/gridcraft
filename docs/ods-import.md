# Importing OpenDocument spreadsheets

Open an `.ods` file with File → Open, drag it onto GridCraft, or pass it to the CLI. GridCraft
imports worksheet data into a new workbook and shows a warning describing the conversion limits.

The import reads sheets, numbers, text, booleans, supported dates/times, repeated cells/rows and
merged cells. Formula cells become their **last saved results**, not live formulas. These results
may be out of date if the source application did not recalculate before saving. Unsupported or
missing values retain available display text and produce a warning.

This is data import, not full ODS compatibility. Source formatting, formulas, charts, images,
names and other workbook features are not preserved.

Save opens Save As and suggests a new `.xlsx` file. The source ODS file is not used as the
workbook's save destination, because saving over it would replace the formatting the import
didn't read. Save As `.ods` writes a new ODS file.

## Saving as ODS

Save As (or `file.saveAs`, `file.saveBytes {format: "ods"}`, `gridcraft-cli convert … out.ods`)
writes ODF 1.3: every sheet with its name and visibility, values with their types (number,
percentage, date, time, boolean, text, error), formulas in OpenFormula with their results, merges,
column widths and row heights (hidden ones too), cell styles (font, size, bold, italic, underline,
strike-through, colour, fill, borders, alignment, wrap, rotation), number formats (numbers,
percentages, scientific, currency text, dates, times, text) and workbook-scoped names. Charts,
images, shapes, conditional formats, validations, PivotTables, comments and print setup are not
written. A formula OpenFormula can't spell (a structured table reference, a spill `#`) is saved
as its value.

```sh
gridcraft-cli info source.ods
gridcraft-cli convert source.ods imported.xlsx
gridcraft-cli convert book.xlsx book.ods
```

CLI import warnings go to stderr, leaving structured output on stdout available for scripts.
