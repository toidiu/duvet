// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::trivial_regex)]

use crate::{
    annotation::AnnotationLevel,
    specification::{Format, Line, Section, Specification},
    target::TargetPath,
    Error,
};
use lazy_static::lazy_static;
use rayon::prelude::*;
use regex::{Regex, RegexSet};
use std::{fs::OpenOptions, io::BufWriter, path::PathBuf};
use structopt::StructOpt;

#[cfg(test)]
mod tests;

lazy_static! {
    static ref KEY_WORDS: Vec<(Regex, AnnotationLevel)> = {
        let matches = [
            ("MUST( NOT)?", AnnotationLevel::Must),
            ("SHALL( NOT)?", AnnotationLevel::Must),
            ("REQUIRED", AnnotationLevel::Must),
            ("SHOULD( NOT)?", AnnotationLevel::Should),
            ("(NOT )?RECOMMENDED", AnnotationLevel::Should),
            ("MAY", AnnotationLevel::May),
            ("OPTIONAL", AnnotationLevel::May),
        ];

        matches
            .iter()
            .cloned()
            .map(|(pat, l)| {
                let r = Regex::new(&format!("\\b{}\\b\"?", pat))?;
                Ok((r, l))
            })
            .collect::<Result<_, Error>>()
            .unwrap()
    };
    static ref KEY_WORDS_SET: RegexSet =
        RegexSet::new(KEY_WORDS.iter().map(|(r, _)| r.as_str())).unwrap();
}

#[derive(Debug, StructOpt)]
pub struct Extract {
    #[structopt(short, long, default_value = "IETF")]
    format: Format,

    #[structopt(short, long, default_value = "toml")]
    extension: String,

    #[structopt(short, long, default_value = ".")]
    out: PathBuf,

    target: TargetPath,
}

impl Extract {
    pub fn exec(&self) -> Result<(), Error> {
        let contents = self.target.load()?;
        let spec = self.format.parse(&contents)?;
        let sections = extract_sections(&spec);
        let local_path = self.target.local();

        if self.out.extension().is_some() {
            // assume a path with an extension is a single file
            // TODO output to single file
            todo!("single file not implemented");
        } else {
            // output to directory
            sections
                .par_iter()
                .map(|(section, features)| {
                    // The specification may be stored alongside the extracted TOML.
                    let mut out = match local_path.strip_prefix(&self.out) {
                        Ok(path) => self.out.join(path),
                        Err(_e) => self.out.join(&local_path),
                    };

                    out.set_extension("");
                    let _ = std::fs::create_dir_all(&out);
                    out.push(format!("{}.{}", section.id, self.extension));

                    let file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(out)?;
                    let mut file = BufWriter::new(file);

                    let target = &self.target;

                    match &self.extension[..] {
                        "rs" => write_rust(&mut file, target, section, features)?,
                        "toml" => write_toml(&mut file, target, section, features)?,
                        ext => unimplemented!("{}", ext),
                    }

                    Ok(())
                })
                .collect::<Result<(), std::io::Error>>()?;
        }

        Ok(())
    }
}

fn extract_sections<'a>(spec: &'a Specification) -> Vec<(&'a Section<'a>, Vec<Feature<'a>>)> {
    spec.sorted_sections()
        .par_iter()
        .map(|section| extract_section(section))
        .filter(|(_section, features)| !features.is_empty())
        .collect()
}

fn extract_section<'a>(section: &'a Section<'a>) -> (&'a Section<'a>, Vec<Feature>) {
    let mut features = vec![];
    let lines = &section.lines[..];

    for (lineno, line) in lines.iter().enumerate() {
        let line = if let Line::Str(l) = line {
            l
        } else {
            continue;
        };

        if KEY_WORDS_SET.is_match(line) {
            for (key_word, level) in KEY_WORDS.iter() {
                for occurance in key_word.find_iter(line) {
                    // filter out any matches in quotes - these are definitions in the
                    // document
                    if occurance.as_str().ends_with('"') {
                        continue;
                    }

                    let mut quote = vec![];

                    let start = find_open(lines, lineno, occurance.start());
                    let end = find_close(lines, lineno, occurance.end());

                    #[allow(clippy::needless_range_loop)]
                    for i in start.0..=end.0 {
                        // The requirement didn't end with a period so stop processing
                        if i == lines.len() {
                            break;
                        }

                        match &lines[i] {
                            Line::Break => {
                                continue;
                            }
                            Line::Str(line) => {
                                let mut line = &line[..];

                                if i == end.0 {
                                    line = &line[..end.1];
                                }

                                if i == start.0 {
                                    line = &line[start.1..];
                                }

                                line = line.trim();

                                if !line.is_empty() {
                                    quote.push(line);
                                }
                            }
                        }
                    }

                    let feature = Feature {
                        level: *level,
                        quote,
                    };

                    // TODO split compound features by level
                    // for now we just add the highest priority level
                    if feature.should_add() {
                        features.push(feature);
                    }
                }
            }
        }
    }

    (section, features)
}

#[derive(Clone, Debug)]
pub struct Feature<'a> {
    level: AnnotationLevel,
    quote: Vec<&'a str>,
}

impl<'a> Feature<'a> {
    pub fn should_add(&self) -> bool {
        match self.compound_level() {
            Some(level) => level == self.level,
            None => true,
        }
    }

    pub fn compound_level(&self) -> Option<AnnotationLevel> {
        KEY_WORDS_SET
            .matches(&self.quote.join("\n"))
            .iter()
            .map(|i| KEY_WORDS[i].1)
            .max()
    }
}

fn find_open(lines: &[Line], lineno: usize, start: usize) -> (usize, usize) {
    let line = &lines[lineno];

    if let Line::Str(line) = line {
        if let Some(offset) = find_open_line(&line[..start]) {
            return (lineno, offset);
        }
    }

    let before = &lines[..lineno];

    if !before.is_empty() {
        return find_next_open(before);
    }

    (lineno, 0)
}

fn find_next_open(lines: &[Line]) -> (usize, usize) {
    let mut open = (lines.len() - 1, 0);

    for (lineno, line) in lines.iter().enumerate().rev() {
        let line = match line {
            Line::Str(l) => l,
            Line::Break => return open,
        };

        // if the line is empty we're at the beginning sentence
        if line.is_empty() {
            return open;
        }

        if let Some(end) = find_open_line(line) {
            return (lineno, end);
        }

        open = (lineno, 0);
    }

    open
}

fn find_open_line(line: &str) -> Option<usize> {
    let end = line.rfind('.')? + 1;

    match line[(end)..].chars().next() {
        Some(' ') | Some('\t') => Some(end),
        None => Some(end),
        _ => find_close_line(&line[..(end - 1)]),
    }
}

fn find_close(lines: &[Line], lineno: usize, end: usize) -> (usize, usize) {
    let line = &lines[lineno];

    if let Line::Str(line) = line {
        if let Some(offset) = find_close_line(&line[end..]) {
            return (lineno, end + offset);
        }
    }

    let after = &lines[lineno..];

    if !after.is_empty() {
        let (mut end_line, end_offset) = find_next_close(&after[1..]);
        end_line += lineno + 1;

        return (end_line, end_offset);
    }

    (lineno, end)
}

fn find_next_close(lines: &[Line]) -> (usize, usize) {
    let mut end = (0, 0);

    for (lineno, line) in lines.iter().enumerate() {
        let line = match line {
            Line::Str(l) => l,
            Line::Break => return (lineno, 0),
        };

        // if the line is empty we're finished with the sentence
        if line.is_empty() {
            return (lineno, 0);
        }

        if let Some(end) = find_close_line(line) {
            return (lineno, end);
        }

        end = (lineno, line.len());
    }

    end
}

fn find_close_line(line: &str) -> Option<usize> {
    let end = line.find('.')? + 1;
    let line = &line[end..];

    match line.chars().next() {
        Some(' ') => Some(end),
        Some('\t') => Some(end),
        None => Some(end),
        _ => {
            let end = end + 1 + find_close_line(&line[1..])?;
            Some(end)
        }
    }
}

fn write_rust<W: std::io::Write>(
    w: &mut W,
    target: &TargetPath,
    section: &Section,
    features: &[Feature],
) -> Result<(), std::io::Error> {
    writeln!(w, "//! {}#{}", target, section.id)?;
    writeln!(w, "//!")?;
    writeln!(w, "//! {}", section.full_title)?;
    writeln!(w, "//!")?;
    for line in &section.lines {
        if let Line::Str(line) = line {
            writeln!(w, "//! {}", line)?;
        }
    }
    writeln!(w)?;

    for feature in features {
        writeln!(w, "//= {}#{}", target, section.id)?;
        writeln!(w, "//= type=spec")?;
        writeln!(w, "//= level={}", feature.level)?;
        for line in feature.quote.iter() {
            writeln!(w, "//# {}", line)?;
        }
        writeln!(w)?;
    }

    Ok(())
}

fn write_toml<W: std::io::Write>(
    w: &mut W,
    target: &TargetPath,
    section: &Section,
    features: &[Feature],
) -> Result<(), std::io::Error> {
    writeln!(w, "target = \"{}#{}\"", target, section.id)?;
    writeln!(w)?;
    writeln!(w, "# {}", section.full_title)?;
    writeln!(w, "#")?;
    for line in &section.lines {
        if let Line::Str(line) = line {
            writeln!(w, "# {}", line)?;
        }
    }
    writeln!(w)?;

    for feature in features {
        writeln!(w, "[[spec]]")?;
        writeln!(w, "level = \"{}\"", feature.level)?;
        writeln!(w, "quote = '''")?;
        for line in feature.quote.iter() {
            writeln!(w, "{}", line)?;
        }
        writeln!(w, "'''")?;
        writeln!(w)?;
    }

    Ok(())
}
