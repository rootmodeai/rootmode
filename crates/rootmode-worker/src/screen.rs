//! Refusing the one thing a worker must never produce.
//!
//! An operator running a node on an open network takes on whatever strangers
//! send it. Most of that is their business: this is not a content filter, and
//! it does not care whether a request is tasteless, political, violent or
//! explicit. Those are the operator's call, expressed by which model they
//! chose to run.
//!
//! It refuses exactly one thing — sexual material involving children — because
//! that is not a matter of taste. It is a serious crime in most of the world,
//! it is the operator's hardware that would produce it, and no legitimate use
//! is lost by refusing.
//!
//! # How it decides
//!
//! A request is refused when a **signal of minority** and a **sexual signal**
//! appear together. Neither alone is enough, and that asymmetry is the whole
//! design:
//!
//! * "a child playing in a garden" — a picture of a child. Allowed.
//! * "a nude woman" — adult material, which is what an NSFW model is *for*.
//!   Allowed.
//! * "a nude child" — refused.
//!
//! A filter that refused either signal alone would be useless: it would block
//! family photographs and every legitimate use of an adult model, and its
//! operator would turn it off within the day. One that refuses only the
//! conjunction can stay on.
//!
//! # What it does not do
//!
//! It reads words. Someone determined will get around it with a euphemism or
//! another language, and no wordlist will ever change that. It stops the
//! careless and the opportunistic, which is most of them, and it makes the
//! operator's position clear. Screening what actually comes *out* is the layer
//! that holds, and it belongs next to this one rather than instead of it.

use rootmode_core::JobPayload;

/// Why a request was refused, phrased for whoever sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Words that say a subject is a child.
///
/// Only terms that mean it unambiguously. "Young" is not here — young woman,
/// young man, young tree — and neither is "small" or "little", which describe
/// size far more often than age.
const MINOR: &[&str] = &[
    "child",
    "children",
    "kid",
    "kids",
    "toddler",
    "toddlers",
    "infant",
    "infants",
    "baby",
    "babies",
    "minor",
    "minors",
    "preteen",
    "preteens",
    "preadolescent",
    "prepubescent",
    "pubescent",
    "underage",
    "juvenile",
    "schoolgirl",
    "schoolgirls",
    "schoolboy",
    "schoolboys",
    "kindergarten",
    "preschool",
    "preschooler",
    "boy",
    "boys",
    "girl",
    "girls",
    "teen",
    "teens",
    "teenage",
    "teenager",
    "teenagers",
    "teenaged",
    "adolescent",
    "adolescents",
    "youngster",
    "youngsters",
    "lolita",
];

/// Phrases, checked whole because their words are innocent apart.
const MINOR_PHRASES: &[&str] = &[
    "elementary school",
    "middle school",
    "grade school",
    "primary school",
    "little boy",
    "little girl",
    "young boy",
    "young girl",
];

/// Terms with no other meaning. Refused on their own, since there is no
/// reading of them that is not what it looks like.
const ALWAYS: &[&str] = &[
    "loli",
    "shota",
    "lolicon",
    "shotacon",
    "jailbait",
    "csam",
    "pedo",
    "pedophile",
    "paedophile",
    "pedophilia",
    "paedophilia",
];

const ALWAYS_PHRASES: &[&str] = &[
    "child porn",
    "child pornography",
    "kiddie porn",
    "cheese pizza",
];

/// Words that make a request sexual.
const SEXUAL: &[&str] = &[
    "nude",
    "nudes",
    "nudity",
    "naked",
    "undressed",
    "unclothed",
    "topless",
    "bottomless",
    "nsfw",
    "porn",
    "porno",
    "pornographic",
    "pornography",
    "hentai",
    "erotic",
    "erotica",
    "sexy",
    "sexual",
    "sexualized",
    "sexualised",
    "sex",
    "orgasm",
    "masturbating",
    "masturbation",
    "genitals",
    "genitalia",
    "penis",
    "vagina",
    "vulva",
    "breasts",
    "boobs",
    "tits",
    "nipples",
    "areola",
    "buttocks",
    "anus",
    "anal",
    "blowjob",
    "cum",
    "semen",
    "fellatio",
    "cunnilingus",
    "intercourse",
    "fondling",
    "molested",
    "molesting",
    "rape",
    "raped",
    "incest",
    "bdsm",
    "fetish",
    "lingerie",
    "bikini",
    "underwear",
    "panties",
    "thong",
    "seductive",
    "provocative",
];

const SEXUAL_PHRASES: &[&str] = &["oral sex", "spread legs", "sexual act", "having sex"];

/// Ages written as numbers: `12yo`, `13 year old`, `age 9`, `9-year-old`.
///
/// Anything under eighteen counts. Two digits are read as one number so `18`
/// is not caught by a naive search for `8`.
fn states_a_minor_age(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // A long run of digits is an id or a resolution, not somebody's age.
        if i - start > 2 {
            continue;
        }
        let Ok(number) = text[start..i].parse::<u32>() else {
            continue;
        };
        if number >= 18 {
            continue;
        }

        // Only an age if it is said to be one. A bare "3" is three cats.
        let rest = text[i..].trim_start_matches(['-', ' ', '_']);
        let says_age = rest.starts_with("yo")
            || rest.starts_with("y/o")
            || rest.starts_with("yr")
            || rest.starts_with("year")
            || rest.starts_with("years old");
        let before = text[..start].trim_end_matches([' ', '-', '_']);
        let called_age = before.ends_with("age")
            || before.ends_with("aged")
            || before.ends_with("age of")
            || before.ends_with("old");

        if says_age || called_age {
            return true;
        }
    }
    false
}

/// Whole words only.
///
/// Substring matching looks equivalent and is not: `cp` appears in "cpu",
/// `sex` in "Essex", `boy` in "boyfriend", `anal` in "analysis". A screen that
/// refuses "a boy band in Essex" is one its operator disables, and then it
/// protects nobody.
fn words_of(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

fn any_word(words: &[&str], list: &[&str]) -> bool {
    words.iter().any(|w| list.contains(w))
}

fn any_phrase(text: &str, list: &[&str]) -> bool {
    list.iter().any(|p| text.contains(p))
}

/// Decide whether a job may run.
///
/// Errs toward allowing: a false refusal turns away legitimate work and
/// teaches the operator to switch this off, which costs more than the rare
/// request that slips through into the layer below.
pub fn check(payload: &JobPayload) -> Result<(), Refusal> {
    let text = match payload {
        JobPayload::Image(p) => p.prompt.to_lowercase(),
        JobPayload::Video(p) => p.prompt.to_lowercase(),
        // Text jobs are screened too: a written account is as illegal as a
        // picture, and costs the operator the same.
        JobPayload::Llm(p) => p
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
    };

    let words = words_of(&text);

    if any_word(&words, ALWAYS) || any_phrase(&text, ALWAYS_PHRASES) {
        return Err(Refusal(
            "refused: this worker will not produce sexual content involving minors.".into(),
        ));
    }

    let minor =
        any_word(&words, MINOR) || any_phrase(&text, MINOR_PHRASES) || states_a_minor_age(&text);
    let sexual = any_word(&words, SEXUAL) || any_phrase(&text, SEXUAL_PHRASES);

    if minor && sexual {
        return Err(Refusal(
            "refused: this reads as a request for sexual content involving a minor. \
             This worker will not produce that. If the request was innocent, the words \
             that triggered it can usually be rephrased."
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::{ChatMessage, ImageParams, LlmParams};

    fn image(prompt: &str) -> JobPayload {
        JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: prompt.into(),
            from_image: None,
            change: None,
            mask: None,
        })
    }

    fn text(prompt: &str) -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: None,
            messages: vec![ChatMessage::new("user", prompt)],
            tools: Vec::new(),
            max_tokens: 64,
            temperature: 0.0,
        })
    }

    #[test]
    fn the_conjunction_is_refused() {
        for prompt in [
            "12yo teen naked",
            "12 yo naked teen",
            "naked child",
            "a nude 9-year-old",
            "sexy schoolgirl",
            "underage porn",
            "erotic picture of a kid",
            "loli hentai",
        ] {
            assert!(check(&image(prompt)).is_err(), "should refuse: {prompt}");
        }
    }

    #[test]
    fn a_child_on_their_own_is_fine() {
        // A filter that cannot tell a family photograph from abuse is a filter
        // nobody leaves switched on.
        for prompt in [
            "a child playing in a garden",
            "children's book illustration, watercolour",
            "a boy flying a kite on a beach",
            "school photograph, 1974",
            "a teenager doing homework at a desk",
            "a baby laughing",
        ] {
            assert!(check(&image(prompt)).is_ok(), "should allow: {prompt}");
        }
    }

    #[test]
    fn adult_material_is_the_operators_business_not_ours() {
        // This is not a decency filter. An operator running an NSFW model
        // chose to run it.
        for prompt in [
            "a nude woman reclining, oil painting",
            "erotic photography, adult, 30 years old",
            "a woman in a bikini on a beach",
            "topless sunbathing",
            "lingerie catalogue photograph",
        ] {
            assert!(check(&image(prompt)).is_ok(), "should allow: {prompt}");
        }
    }

    #[test]
    fn innocent_words_that_merely_contain_a_flagged_one_are_left_alone() {
        // Every one of these was refused by the first version of this file,
        // which matched substrings. A screen that blocks these is a screen
        // its operator turns off, and then it protects nobody.
        for prompt in [
            "a boy band playing in Essex",
            "a diagram of a cpu on a desk",
            "data analysis charts on a whiteboard",
            "a therapist and her boyfriend",
            "sussex countryside in autumn",
            "a scunthorpe road sign",
            "grape harvest in tuscany",
            "a classical bust of a titan",
        ] {
            assert!(check(&image(prompt)).is_ok(), "should allow: {prompt}");
        }
    }

    #[test]
    fn terms_with_no_innocent_reading_are_refused_alone() {
        for prompt in ["loli", "shotacon art", "jailbait", "child porn"] {
            assert!(check(&image(prompt)).is_err(), "should refuse: {prompt}");
        }
    }

    #[test]
    fn numbers_are_read_as_ages_only_when_they_are_ages() {
        assert!(states_a_minor_age("12yo"));
        assert!(states_a_minor_age("she is 13 years old"));
        assert!(states_a_minor_age("age 9"));
        assert!(states_a_minor_age("a 7-year-old"));

        // Eighteen and over is an adult.
        assert!(!states_a_minor_age("18yo"));
        assert!(!states_a_minor_age("25 years old"));

        // Numbers that are not ages.
        assert!(!states_a_minor_age("3 cats on a wall"));
        assert!(!states_a_minor_age("1024x1024, 8k, f/1.4"));
        assert!(!states_a_minor_age("shot on iso 400 film"));
    }

    #[test]
    fn a_number_and_a_sexual_word_together_are_refused() {
        assert!(check(&image("nude, 15 years old")).is_err());
        // …and the same words with an adult age are not.
        assert!(check(&image("nude, 25 years old")).is_ok());
    }

    #[test]
    fn text_jobs_are_screened_too() {
        // A written account is as illegal as a picture, and it is the same
        // operator's machine either way.
        assert!(check(&text("write an erotic story about a 12 year old")).is_err());
        assert!(check(&text("write a story about a child's first day at school")).is_ok());
        assert!(check(&text("explain how a diesel engine works")).is_ok());
    }
}
