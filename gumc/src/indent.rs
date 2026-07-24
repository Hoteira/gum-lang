struct OutLine {
    code: String,
    comment: Option<String>,
}

fn push_closer(lines: &mut Vec<OutLine>) {
    if let Some(last) = lines.last_mut() {
        last.code.push_str(" }");
    }
}

pub fn preprocess(source: &str) -> Result<String, String> {
    let mut stack: Vec<usize> = vec![0];
    let mut pending_open = false;
    let mut unsafe_header_indent: Option<usize> = None;
    let mut lines: Vec<OutLine> = Vec::new();

    for (line_no, raw_line) in source.lines().enumerate() {
        let stripped = raw_line.trim();

        if stripped.is_empty() || stripped.starts_with("//") {
            let (code, comment) = split_trailing_comment(raw_line);
            lines.push(OutLine {
                code: code.to_string(),
                comment: comment.map(str::to_string),
            });
            continue;
        }

        let indent = leading_indent(raw_line);

        if let Some(header_indent) = unsafe_header_indent {
            if indent > header_indent {
                let (code, comment) = split_trailing_comment(raw_line);
                lines.push(OutLine {
                    code: code.trim_end().to_string(),
                    comment: comment.map(str::to_string),
                });
                continue;
            }
            push_closer(&mut lines);
            unsafe_header_indent = None;
        }

        if pending_open {
            if indent > *stack.last().unwrap() {
                stack.push(indent);
            } else {
                push_closer(&mut lines);
            }
            pending_open = false;
        }

        while indent < *stack.last().unwrap() {
            stack.pop();
            push_closer(&mut lines);
        }

        if indent != *stack.last().unwrap() {
            return Err(format!(
                "Indentation error on line {}: inconsistent indent (got {}, expected {})",
                line_no + 1,
                indent,
                stack.last().unwrap()
            ));
        }

        let (code, comment) = split_trailing_comment(raw_line.trim_end());
        let code_trimmed = code.trim();

        let mut emitted = " ".repeat(indent);
        if code_trimmed.ends_with(':') {
            let header = code_trimmed[..code_trimmed.len() - 1].trim_end();
            emitted.push_str(header);
            emitted.push_str(" {");
            if header.trim() == "unsafe" {
                unsafe_header_indent = Some(indent);
            } else {
                pending_open = true;
            }
        } else if is_attribute(code_trimmed) {
            emitted.push_str(code_trimmed);
        } else {
            emitted.push_str(code_trimmed);
            emitted.push(';');
        }
        lines.push(OutLine {
            code: emitted,
            comment: comment.map(str::to_string),
        });
    }

    if unsafe_header_indent.is_some() {
        push_closer(&mut lines);
    }
    if pending_open {
        push_closer(&mut lines);
    }
    while stack.len() > 1 {
        stack.pop();
        push_closer(&mut lines);
    }

    let mut out = String::new();
    for line in lines {
        out.push_str(&line.code);
        if let Some(c) = line.comment {
            if !line.code.is_empty() && !line.code.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(&c);
        }
        out.push('\n');
    }
    Ok(out)
}

fn is_attribute(code: &str) -> bool {
    code.starts_with('[') && code.ends_with(']')
}

fn leading_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn split_trailing_comment(line: &str) -> (&str, Option<&str>) {
    let mut in_string = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            b'/' if !in_string && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return (&line[..i], Some(&line[i..]));
            }
            _ => {}
        }
        i += 1;
    }
    (line, None)
}
