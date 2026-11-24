//! Markdown for assistant messages in the native chat.

use super::theme::*;
use eframe::egui::{
    self, text::LayoutJob, CornerRadius, CursorIcon, Label, Margin, RichText, Sense, Stroke, Ui,
};
use pulldown_cmark::{Alignment, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use std::{iter::Peekable, ops::Range};

const PROSE_SIZE: f32 = 15.0;

pub(super) fn markdown(ui: &mut Ui, text: &str) {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    blocks(
        ui,
        &mut Parser::new_ext(text, options).peekable(),
        None,
        false,
    );
}

#[derive(Clone, Copy, Default)]
struct InlineStyle {
    strong: bool,
    emphasis: bool,
    strike: bool,
}

struct Inline<'a> {
    job: LayoutJob,
    size: f32,
    base_strong: bool,
    style: InlineStyle,
    stack: Vec<InlineStyle>,
    link: Option<(CowStr<'a>, usize)>,
    links: Vec<(Range<usize>, CowStr<'a>)>,
    chars: usize,
}

impl<'a> Inline<'a> {
    fn new(size: f32, base_strong: bool) -> Self {
        Self {
            job: LayoutJob::default(),
            size,
            base_strong,
            style: InlineStyle::default(),
            stack: Vec::new(),
            link: None,
            links: Vec::new(),
            chars: 0,
        }
    }

    fn append(&mut self, text: &str, code: bool) {
        let font_id = if code {
            mono(self.size - 1.0)
        } else if self.base_strong || self.style.strong {
            bold(self.size)
        } else {
            body(self.size)
        };
        let color = if self.link.is_some() { INFO } else { INK };
        let style = egui::TextFormat {
            font_id,
            line_height: Some(self.size + 7.0),
            color,
            background: if code {
                PANEL
            } else {
                egui::Color32::TRANSPARENT
            },
            italics: self.style.emphasis,
            underline: if self.link.is_some() {
                Stroke::new(1.0, INFO)
            } else {
                Stroke::NONE
            },
            strikethrough: if self.style.strike {
                Stroke::new(1.0, INK2)
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        self.job.append(text, 0.0, style);
        self.chars += text.chars().count();
    }

    fn event(&mut self, event: Event<'a>) {
        match event {
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.append(&text, false)
            }
            Event::Code(code) => self.append(&code, true),
            Event::SoftBreak => self.append(" ", false),
            Event::HardBreak => self.append("\n", false),
            Event::TaskListMarker(checked) => {
                self.append(if checked { "☑  " } else { "☐  " }, false)
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if dest_url.starts_with("https://") || dest_url.starts_with("http://") {
                    self.link = Some((dest_url, self.chars));
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, start)) = self.link.take() {
                    self.links.push((start..self.chars, url));
                }
            }
            Event::Start(tag @ (Tag::Strong | Tag::Emphasis | Tag::Strikethrough)) => {
                self.stack.push(self.style);
                match tag {
                    Tag::Strong => self.style.strong = true,
                    Tag::Emphasis => self.style.emphasis = true,
                    _ => self.style.strike = true,
                }
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                if let Some(style) = self.stack.pop() {
                    self.style = style;
                }
            }
            _ => {}
        }
    }

    fn show(self, ui: &mut Ui, align: egui::Align) {
        if self.job.text.is_empty() {
            return;
        }
        if self.links.is_empty() {
            ui.add(Label::new(self.job).wrap().halign(align).selectable(true));
            return;
        }

        // Use the same wrapped galley for selection and the link hit areas.
        let mut job = self.job;
        job.wrap.max_width = ui.available_width();
        job.halign = align;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        let response = ui.add(
            Label::new(galley.clone())
                .selectable(true)
                .sense(Sense::click()),
        );
        let origin = match align {
            egui::Align::Min => response.rect.left_top(),
            egui::Align::Center => response.rect.center_top(),
            egui::Align::Max => response.rect.right_top(),
        };
        if let Some(pos) = response.hover_pos() {
            let pos = pos - origin.to_vec2();
            let mut offset = 0;
            for row in &galley.rows {
                let local_x = pos.x - row.pos.x;
                if row.rect().contains(pos) {
                    for (index, glyph) in row.glyphs.iter().enumerate() {
                        if (glyph.pos.x..glyph.max_x()).contains(&local_x) {
                            if let Some((_, url)) = self
                                .links
                                .iter()
                                .find(|(range, _)| range.contains(&(offset + index)))
                            {
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                if response.clicked() {
                                    ui.ctx().open_url(egui::OpenUrl::new_tab(url.as_ref()));
                                }
                                response.on_hover_text(url.as_ref());
                                return;
                            }
                        }
                    }
                }
                offset += row.glyphs.len() + usize::from(row.ends_with_newline);
            }
        }
    }
}

type Events<'a> = Peekable<Parser<'a>>;

fn inline<'a>(events: &mut Events<'a>, end: TagEnd, size: f32, strong: bool) -> Inline<'a> {
    let mut line = Inline::new(size, strong);
    for event in events {
        if event == Event::End(end) {
            break;
        }
        line.event(event);
    }
    line
}

fn blocks(ui: &mut Ui, events: &mut Events<'_>, end: Option<TagEnd>, compact: bool) {
    let mut line = Inline::new(PROSE_SIZE, false);
    while let Some(event) = events.next() {
        match event {
            Event::End(tag) if Some(tag) == end => break,
            Event::End(TagEnd::Paragraph)
            | Event::Start(Tag::Heading { .. })
            | Event::Start(Tag::List(_) | Tag::BlockQuote(_) | Tag::CodeBlock(_) | Tag::Table(_))
            | Event::Rule => {
                let pending = std::mem::replace(&mut line, Inline::new(PROSE_SIZE, false));
                pending.show(ui, egui::Align::Min);
                match event {
                    Event::Start(Tag::Heading { level, .. }) => {
                        let size = (25.0 - level as u8 as f32 * 2.0).max(16.0);
                        ui.add_space(4.0);
                        inline(events, TagEnd::Heading(level), size, true)
                            .show(ui, egui::Align::Min);
                    }
                    Event::Start(Tag::List(start)) => list(ui, events, start),
                    Event::Start(Tag::BlockQuote(_)) => {
                        egui::Frame::new()
                            .fill(PANEL)
                            .corner_radius(CornerRadius::same(R))
                            .inner_margin(Margin::symmetric(12, 5))
                            .show(ui, |ui| {
                                blocks(ui, events, Some(TagEnd::BlockQuote(None)), false);
                            });
                    }
                    Event::Start(Tag::CodeBlock(kind)) => {
                        let mut code = String::new();
                        for event in events.by_ref() {
                            match event {
                                Event::End(TagEnd::CodeBlock) => break,
                                Event::Text(text) => code.push_str(&text),
                                _ => {}
                            }
                        }
                        let language = match &kind {
                            CodeBlockKind::Fenced(info) => {
                                info.split_whitespace().next().unwrap_or("Código")
                            }
                            CodeBlockKind::Indented => "Código",
                        };
                        code_block(ui, language, &code);
                    }
                    Event::Start(Tag::Table(alignments)) => table(ui, events, &alignments),
                    Event::Rule => {
                        ui.separator();
                    }
                    _ => {}
                }
                if !compact {
                    ui.add_space(4.0);
                }
            }
            _ => line.event(event),
        }
    }
    line.show(ui, egui::Align::Min);
}

fn list(ui: &mut Ui, events: &mut Events<'_>, start: Option<u64>) {
    let mut number = start;
    while let Some(Event::Start(Tag::Item)) = events.peek() {
        events.next();
        let marker = number.map_or_else(|| "•".to_owned(), |n| format!("{n}."));
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(marker).font(medium(PROSE_SIZE)).color(INK2));
            ui.vertical(|ui| blocks(ui, events, Some(TagEnd::Item), true));
        });
        number = number.map(|n| n.saturating_add(1));
    }
    if matches!(events.peek(), Some(Event::End(TagEnd::List(_)))) {
        events.next();
    }
}

fn table(ui: &mut Ui, events: &mut Events<'_>, alignments: &[Alignment]) {
    let cell_width = (ui.available_width() / alignments.len() as f32 - 16.0).max(105.0);
    egui::ScrollArea::horizontal().show(ui, |ui| {
        while let Some(event) = events.next() {
            let head = match event {
                Event::Start(Tag::TableHead) => true,
                Event::Start(Tag::TableRow) => false,
                Event::End(TagEnd::Table) => break,
                _ => continue,
            };
            egui::Frame::new()
                .fill(if head { RAISED } else { PANEL })
                .inner_margin(Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        for alignment in alignments {
                            if !matches!(events.peek(), Some(Event::Start(Tag::TableCell))) {
                                break;
                            }
                            events.next();
                            let align = match alignment {
                                Alignment::Center => egui::Align::Center,
                                Alignment::Right => egui::Align::Max,
                                _ => egui::Align::Min,
                            };
                            ui.allocate_ui_with_layout(
                                egui::vec2(cell_width, 0.0),
                                egui::Layout::top_down(align),
                                |ui| {
                                    ui.set_width(cell_width);
                                    inline(events, TagEnd::TableCell, 13.5, head).show(ui, align);
                                },
                            );
                        }
                    });
                });
        }
    });
}

const COPY_FEEDBACK_SECS: f64 = 2.0;

fn copy_done_button(ui: &mut Ui) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for (st, fill_alpha, stroke_alpha) in [
            (&mut w.inactive, 0.14, 0.55),
            (&mut w.hovered, 0.22, 0.85),
            (&mut w.active, 0.28, 1.0),
            (&mut w.open, 0.22, 0.85),
        ] {
            let fill = RUN.gamma_multiply(fill_alpha);
            st.bg_fill = fill;
            st.weak_bg_fill = fill;
            st.bg_stroke = Stroke::new(1.0, RUN.gamma_multiply(stroke_alpha));
            st.fg_stroke = Stroke::new(1.0, RUN);
            st.corner_radius = CornerRadius::same(R);
        }
        ui.add(egui::Button::new(
            RichText::new("✓ Copiado").font(medium(12.5)),
        ))
    })
    .inner
    .on_hover_text("Copiado al portapapeles")
}

pub(super) fn code_block(ui: &mut Ui, label: &str, code: &str) {
    let width = ui.available_width();
    let display = code
        .strip_suffix("\r\n")
        .or_else(|| code.strip_suffix('\n'))
        .unwrap_or(code);
    let copy_id = ui.scope_id().with(("code_copy_at", ui.next_auto_id()));
    let now_t = ui.input(|i| i.time);
    let copied_at = ui.ctx().data(|d| d.get_temp::<f64>(copy_id));
    let just_copied = copied_at.is_some_and(|t| now_t >= t && (now_t - t) < COPY_FEEDBACK_SECS);
    if just_copied {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }

    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(Stroke::new(1.0, RULE))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width((width - 22.0).max(1.0));
            ui.horizontal(|ui| {
                ui.set_width(ui.available_width());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let btn = if just_copied {
                        copy_done_button(ui)
                    } else {
                        quiet(ui, "Copiar").on_hover_text("Copiar al portapapeles")
                    };
                    if btn.clicked() {
                        ui.ctx().copy_text(display.to_owned());
                        ui.ctx().data_mut(|d| d.insert_temp(copy_id, now_t));
                        ui.ctx().request_repaint();
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            Label::new(RichText::new(label).font(medium(12.0)).color(INK2))
                                .truncate(),
                        );
                    });
                });
            });
            ui.separator();
            ui.spacing_mut().scroll.floating = false;
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.add(
                    Label::new(RichText::new(display).font(mono(12.5)).color(INK))
                        .extend()
                        .selectable(true),
                );
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, CentralPanel, Context, RawInput, Rect};

    #[test]
    fn wide_markdown_elements_do_not_expand_the_transcript() {
        let long = "un_tramo_muy_largo_".repeat(30);
        let source = format!(
            "# Respuesta\n\nUn párrafo con **negrita**, *cursiva*, [un enlace](https://example.com) y `{long}`.\n\n\
             9. Primera propuesta\n   - Detalle de la propuesta: {long}\n10. Segunda propuesta\n\n\
             > Una cita con {long}\n\n\
             | Tipo | Resultado | Estado |\n| --- | --- | --- |\n| Texto | {long} | Listo |\n\n\
             ~~~rust\nlet value = \"{long}\";\n"
        );
        for width in [330.0, 760.0] {
            let ctx = Context::default();
            install(&ctx);
            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 600.0));
            let mut output = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::no_frame().show(ui, |ui| {
                        let result = egui::ScrollArea::vertical().show(ui, |ui| {
                            markdown(ui, &source);
                        });
                        assert!(
                            result.content_size.x <= width + 1.0,
                            "content width {} exceeds viewport width {}",
                            result.content_size.x,
                            width
                        );
                    });
                },
            );
            output.textures_delta.clear();
            assert!(output.shapes.iter().any(|shape| matches!(
                &shape.shape, egui::Shape::Text(text) if text.galley.text() == "Respuesta"
            )));
        }
    }

    #[test]
    fn only_web_links_are_clickable_and_nested_styles_are_retained() {
        let mut events = Parser::new(
            "**texto *anidado*** [seguro](https://example.com?a=1&amp;b=2) [local](file:///test.exe)"
        ).peekable();
        events.next();
        let line = inline(&mut events, TagEnd::Paragraph, PROSE_SIZE, false);
        assert_eq!(line.job.text, "texto anidado seguro local");
        assert_eq!(line.links.len(), 1);
        assert_eq!(line.links[0].0, 14..20);
        assert_eq!(line.links[0].1.as_ref(), "https://example.com?a=1&b=2");
        assert!(line
            .job
            .sections
            .iter()
            .any(|s| s.format.italics && s.format.font_id == bold(PROSE_SIZE)));
    }

    #[test]
    fn code_block_copy_button_hovers_clicks_copies_and_shows_feedback() {
        let ctx = Context::default();
        install(&ctx);
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(760.0, 500.0));
        let source = "~~~powershell\npowershell -ExecutionPolicy Bypass -File instalar_rust.ps1\n~~~\n\n~~~bash\necho segundo\n~~~\n";

        let find_button_rect = |output: &egui::FullOutput, label: &str| -> Option<Rect> {
            output.shapes.iter().find_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text() == label => Some(t.visual_bounding_rect()),
                _ => None,
            })
        };

        let run_frame = |time: f64, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(
                RawInput {
                    screen_rect: Some(screen),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    CentralPanel::no_frame().show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                markdown(ui, source);
                            });
                    });
                },
            );
            out.textures_delta.clear();
            out
        };

        // Frame 1: locate the first "Copiar" button.
        let out1 = run_frame(1.0, vec![]);
        let btn_rect = find_button_rect(&out1, "Copiar").expect("Copiar button rendered");
        let btn_pos = btn_rect.center();

        // Frame 2: hover over the first "Copiar" button -> pointing hand cursor + RAISED hover fill.
        let out2 = run_frame(1.1, vec![egui::Event::PointerMoved(btn_pos)]);
        assert_eq!(out2.platform_output.cursor_icon, CursorIcon::PointingHand);
        let has_hover_fill = out2.shapes.iter().any(|cs| match &cs.shape {
            egui::Shape::Rect(r) => r.rect.contains(btn_pos) && r.fill == RAISED,
            _ => false,
        });
        assert!(
            has_hover_fill,
            "hovering Copiar should paint RAISED hover fill"
        );

        // Frame 3: press primary button.
        let _ = run_frame(
            1.2,
            vec![
                egui::Event::PointerMoved(btn_pos),
                egui::Event::PointerButton {
                    pos: btn_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );

        // Frame 4: release primary button -> emits CopyText without trailing newline.
        let out4 = run_frame(
            1.3,
            vec![
                egui::Event::PointerMoved(btn_pos),
                egui::Event::PointerButton {
                    pos: btn_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let copied: Vec<String> = out4
            .platform_output
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                egui::OutputCommand::CopyText(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            copied,
            vec!["powershell -ExecutionPolicy Bypass -File instalar_rust.ps1".to_string()]
        );

        // Frame 5: first block shows "✓ Copiado", second block still shows "Copiar".
        let out5 = run_frame(1.5, vec![egui::Event::PointerMoved(btn_pos)]);
        assert!(
            find_button_rect(&out5, "✓ Copiado").is_some(),
            "first code block should show '✓ Copiado' feedback"
        );
        assert!(
            find_button_rect(&out5, "Copiar").is_some(),
            "second code block should remain 'Copiar'"
        );

        // Frame 6: after feedback duration elapses, label reverts to "Copiar".
        let out6 = run_frame(4.0, vec![]);
        assert!(find_button_rect(&out6, "✓ Copiado").is_none());
        assert!(find_button_rect(&out6, "Copiar").is_some());
    }
}
