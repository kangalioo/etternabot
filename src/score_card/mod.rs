//! Utility code used by various parts of the bot to show a score card

mod replay_graph;

use super::Context;
use crate::{serenity, Error};
use etterna::{SimpleReplay as _, Wife as _};

pub struct ScoreCard<'a> {
	pub scorekey: &'a etterna::Scorekey,
	pub user_id: Option<u32>,      // pass None if score link shouldn't be shown
	pub username: Option<&'a str>, // used to detect scorekey collision
	pub show_ssrs_and_judgements_and_modifiers: bool,
	pub alternative_judge: Option<&'a etterna::Judge>,
}

struct ScoringSystemComparison {
	wife2_score: etterna::Wifescore,
	wife3_score: etterna::Wifescore,
	wife3_score_zero_mean: etterna::Wifescore,
}

struct ReplayAnalysis {
	replay_graph_path: &'static str,
	scoring_system_comparison_j4: ScoringSystemComparison,
	scoring_system_comparison_alternative: Option<ScoringSystemComparison>,
	fastest_finger_jackspeed: f32, // NPS, single finger
	fastest_nps: f32,
	longest_100_combo: u32,
	longest_marv_combo: u32,
	longest_perf_combo: u32,
	longest_combo: u32,
	mean_offset: f32,
	fun_facts: Vec<String>,
}

fn fastest_nps(replay: &etternaonline_api::Replay) -> Option<f32> {
	let note_and_hit_seconds = replay.split_into_notes_and_hits()?;
	let unsorted_hit_seconds = note_and_hit_seconds.hit_seconds;

	let mut sorted_hit_seconds = unsorted_hit_seconds;
	sorted_hit_seconds.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
	let sorted_hit_seconds = sorted_hit_seconds;
	let fastest_nps = etterna::find_fastest_note_subset(&sorted_hit_seconds, 100, 100).speed;

	Some(fastest_nps)
}

fn max_finger_nps(replay: &etternaonline_api::Replay) -> Option<f32> {
	// in the following, DONT scale find_fastest_note_subset results by rate - I only needed
	// to do that for etterna-graph where the note seconds where unscaled. EO's note seconds
	// _are_ scaled though.

	let lanes = replay.split_into_lanes()?;
	let mut max_finger_nps = 0.0;
	for lane in &lanes {
		let mut hit_seconds = lane.hit_seconds.clone();
		// required because EO is jank
		hit_seconds.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

		let this_fingers_max_nps = etterna::find_fastest_note_subset(&hit_seconds, 20, 20).speed;

		if this_fingers_max_nps > max_finger_nps {
			max_finger_nps = this_fingers_max_nps;
		}
	}

	Some(max_finger_nps)
}

fn adjust_offset(replay: &etternaonline_api::Replay) -> (f32, etternaonline_api::Replay) {
	let mean_offset = replay.mean_deviation();
	let replay_zero_mean = etternaonline_api::Replay {
		notes: replay
			.notes
			.iter()
			.map(|note| {
				let mut note = note.clone();
				if let etterna::Hit::Hit { deviation } = &mut note.hit {
					*deviation -= mean_offset;
				}
				note
			})
			.collect(),
	};

	(mean_offset, replay_zero_mean)
}

fn make_scoring_system_comparison(
	score: &etternaonline_api::v1::ScoreData,
	replay: &etternaonline_api::Replay,
	replay_zero_mean: &etternaonline_api::Replay,
	judge: &etterna::Judge,
) -> Option<ScoringSystemComparison> {
	Some(ScoringSystemComparison {
		wife2_score: etternaonline_api::rescore::<etterna::NaiveScorer, etterna::Wife2>(
			replay,
			score.judgements.hit_mines,
			score.judgements.let_go_holds + score.judgements.missed_holds,
			judge,
		)?,
		wife3_score: etternaonline_api::rescore::<etterna::NaiveScorer, etterna::Wife3>(
			replay,
			score.judgements.hit_mines,
			score.judgements.let_go_holds + score.judgements.missed_holds,
			judge,
		)?,
		wife3_score_zero_mean: etternaonline_api::rescore::<etterna::NaiveScorer, etterna::Wife3>(
			replay_zero_mean,
			score.judgements.hit_mines,
			score.judgements.let_go_holds + score.judgements.missed_holds,
			judge,
		)?,
	})
}

/// Returns the wife points for the replay note and the number of notes it should be counted as
/// (0 or 1)
fn replay_note_wife_points(note: &etternaonline_api::ReplayNote) -> (f32, u32) {
	let mut wife_points = 0.0;
	let mut num_notes = 0;

	// I have zero idea what I am doing
	// (more specifically: I have zero idea if and how EO represents mines or holds in replays and
	// whether the way I'm handling them here is remotely correct)
	match note.note_type.unwrap_or(etterna::NoteType::Tap) {
		etterna::NoteType::Tap | etterna::NoteType::HoldHead | etterna::NoteType::Lift => {
			wife_points = etterna::wife3(note.hit, etterna::J4);
			num_notes = 1;
		}
		etterna::NoteType::HoldTail => {
			if note.hit.is_considered_miss(etterna::J4) {
				wife_points = etterna::Wife3::HOLD_DROP_WEIGHT;
			}
		}
		etterna::NoteType::Mine => {
			if let etterna::Hit::Hit { .. } = note.hit {
				wife_points = etterna::Wife3::MINE_HIT_WEIGHT;
			}
		}
		etterna::NoteType::Keysound | etterna::NoteType::Fake => {}
	}

	(wife_points, num_notes)
}

fn calculate_hand_wifescores(replay: &etternaonline_api::Replay) -> (f32, f32) {
	let mut left_wife_points = 0.0;
	let mut left_num_notes = 0;
	let mut right_wife_points = 0.0;
	let mut right_num_notes = 0;

	for note in &replay.notes {
		let (wife_points, num_notes) = match note.lane {
			Some(0 | 1) => (&mut left_wife_points, &mut left_num_notes),
			Some(2 | 3) => (&mut right_wife_points, &mut right_num_notes),
			_ => continue,
		};

		let (note_wife_points, note_num_notes) = replay_note_wife_points(note);
		*wife_points += note_wife_points;
		*num_notes += note_num_notes;
	}

	let left_wifescore = left_wife_points / left_num_notes as f32;
	let right_wifescore = right_wife_points / right_num_notes as f32;

	(left_wifescore, right_wifescore)
}

fn make_left_right_hand_difference_fun_fact(
	fun_facts: &mut Vec<String>,
	replay: &etternaonline_api::Replay,
) {
	let (left_wifescore, right_wifescore) = calculate_hand_wifescores(replay);

	let (better_hand, better_hand_name, lower_hand, lower_hand_name) =
		if left_wifescore > right_wifescore {
			(left_wifescore, "left", right_wifescore, "right")
		} else {
			(right_wifescore, "right", left_wifescore, "left")
		};

	// Check if one hand played twice as good as the other
	if (1.0 - lower_hand) / (1.0 - better_hand) >= 2.0 {
		fun_facts.push(format!(
			"Your {} hand played {:.02}% better than your {} hand ({:.02}% vs {:.02}%. Are you {}-handed? ;)",
			better_hand_name,
			(better_hand - lower_hand) * 100.0,
			lower_hand_name,
			(better_hand) * 100.0,
			(lower_hand) * 100.0,
			better_hand_name,
		));
	}
}

/*
fn make_hit_outliers_fun_fact(fun_facts: &mut Vec<String>, replay: &etternaonline_api::Replay) {
	let mut notes = replay
		.notes
		.iter()
		.map(replay_note_wife_points)
		.collect::<Vec<_>>();

	let mut total_wifepoints = 0.0;
	let mut total_num_notes = 0;
	for (wifepoints, num_notes) in &notes {
		total_wifepoints += wifepoints;
		total_num_notes += num_notes;
	}

	#[derive(PartialOrd, PartialEq)]
	struct NoisyFloat(f32);
	// https://github.com/rust-lang/rust-clippy/issues/6219
	#[allow(clippy::derive_ord_xor_partial_ord)]
	impl Ord for NoisyFloat {
		fn cmp(&self, other: &Self) -> std::cmp::Ordering {
			self.0.partial_cmp(&other.0).unwrap()
		}
	}
	impl Eq for NoisyFloat {}

	// Sort descendingly by the wifescore we'd have with the note excluded
	notes.sort_by_cached_key(|(wifepoints, num_notes)| {
		let new_wifescore = (total_wifepoints - wifepoints) / (total_num_notes - num_notes) as f32;
		std::cmp::Reverse(NoisyFloat(new_wifescore))
	});

	// Now, starting with most negatively impactful notes, see how many we need to exclude to get a
	// sudden jump in wifescore
	let old_wifescore = total_wifepoints / total_num_notes as f32;
	for (i, (wifepoints, num_notes)) in notes.iter().take(10).enumerate() {
		total_wifepoints -= wifepoints;
		total_num_notes -= num_notes;
		let new_wifescore = total_wifepoints / total_num_notes as f32;

		// yes i like meth ehh i mean math
		let excluded_note_proportion = (i + 1) as f32 / total_num_notes as f32;
		let multiplier_threshold = 1.0 / (1.0 - excluded_note_proportion).powf(100.0);
		println!(
			"With {:.02}% excluded, the new score needs to be {:.02}x better (is {:.02}x)",
			excluded_note_proportion * 100.0,
			multiplier_threshold,
			(1.0 - old_wifescore) / (1.0 - new_wifescore)
		);
		if (1.0 - old_wifescore) / (1.0 - new_wifescore) >= multiplier_threshold {
			fun_facts.push(format!(
				"Would have been {:.02}% (instead of {:.02}%) without those {} pesky outliers",
				new_wifescore * 100.0,
				old_wifescore * 100.0,
				i + 1,
			));
			break;
		}
	}
}
*/

fn do_replay_analysis(
	score: &etternaonline_api::v1::ScoreData,
	alternative_judge: Option<&etterna::Judge>,
) -> Option<Result<ReplayAnalysis, Error>> {
	let replay = score.replay.as_ref()?;

	let r = replay_graph::generate_replay_graph(replay, "replay_graph.png").transpose()?;
	if let Err(e) = r {
		return Some(Err(e.into()));
	}

	let (mean_offset, replay_zero_mean) = adjust_offset(replay);

	let mut fun_facts = Vec::new();
	make_left_right_hand_difference_fun_fact(&mut fun_facts, replay);
	// make_hit_outliers_fun_fact(&mut fun_facts, replay);

	Some(Ok(ReplayAnalysis {
		replay_graph_path: "replay_graph.png",
		scoring_system_comparison_j4: make_scoring_system_comparison(
			score,
			replay,
			&replay_zero_mean,
			etterna::J4,
		)?,
		scoring_system_comparison_alternative: match alternative_judge {
			Some(alternative_judge) => Some(make_scoring_system_comparison(
				score,
				replay,
				&replay_zero_mean,
				alternative_judge,
			)?),
			None => None,
		},
		fastest_finger_jackspeed: max_finger_nps(replay)?,
		fastest_nps: fastest_nps(replay)?,
		longest_100_combo: replay.longest_combo(|hit| hit.is_within_window(0.005)),
		longest_marv_combo: replay
			.longest_combo(|hit| hit.is_within_window(etterna::J4.marvelous_window)),
		longest_perf_combo: replay
			.longest_combo(|hit| hit.is_within_window(etterna::J4.perfect_window)),
		longest_combo: replay.longest_combo(|hit| hit.is_within_window(etterna::J4.great_window)),
		mean_offset,
		fun_facts,
	}))
}

fn write_score_card_body(
	info: &ScoreCard<'_>,
	score: &etternaonline_api::v1::ScoreData,
	alternative_judge_wifescore: Option<etterna::Wifescore>,
) -> String {
	let mut description = String::new();

	if let Some(expected_username) = info.username {
		if !score.user.username.eq_ignore_ascii_case(expected_username) {
			description += "**_Multiple scores were assigned the same unique identifier (scorekey), so you are seeing the wrong score here. Sorry!_**\n";
		}
	}

	if let Some(user_id) = info.user_id {
		description += &format!(
			"https://etternaonline.com/score/view/{}{}\n",
			info.scorekey, user_id
		);
	}

	if info.show_ssrs_and_judgements_and_modifiers {
		description += &format!("```\n{}\n```", score.modifiers);
	}

	description += "```nim\n";
	description += &if let Some(alternative_judge_wifescore) = alternative_judge_wifescore {
		format!(
			concat!(
				"        Wife: {:<5.2}%  ⏐\n",
				"     Wife {}: {:<5.2}%  ⏐      Marvelous: {}",
			),
			score.wifescore.as_percent(),
			// UWNRAP: if alternative_judge_wifescore is Some, info.alternative_judge is too
			info.alternative_judge.unwrap().name,
			alternative_judge_wifescore.as_percent(),
			score.judgements.marvelouses,
		)
	} else {
		format!(
			"        Wife: {:<5.2}%  ⏐      Marvelous: {}",
			score.wifescore.as_percent(),
			score.judgements.marvelouses,
		)
	};
	description += &format!(
		"
   Max Combo: {:<5.0}   ⏐        Perfect: {}
     Overall: {:<5.2}   ⏐          Great: {}
      Stream: {:<5.2}   ⏐           Good: {}
     Stamina: {:<5.2}   ⏐            Bad: {}
  Jumpstream: {:<5.2}   ⏐           Miss: {}
  Handstream: {:<5.2}   ⏐      Hit Mines: {}
       Jacks: {:<5.2}   ⏐     Held Holds: {}
   Chordjack: {:<5.2}   ⏐  Dropped Holds: {}
   Technical: {:<5.2}   ⏐   Missed Holds: {}
```
",
		score.max_combo,
		score.judgements.perfects,
		score.ssr.overall,
		score.judgements.greats,
		score.ssr.stream,
		score.judgements.goods,
		score.ssr.stamina,
		score.judgements.bads,
		score.ssr.jumpstream,
		score.judgements.misses,
		score.ssr.handstream,
		score.judgements.hit_mines,
		score.ssr.jackspeed,
		score.judgements.held_holds,
		score.ssr.chordjack,
		score.judgements.let_go_holds,
		score.ssr.technical,
		score.judgements.missed_holds,
	);

	description
}

fn generate_score_comparisons_text(
	score: &etternaonline_api::v1::ScoreData,
	analysis: &ReplayAnalysis,
	alternative_judge: Option<&etterna::Judge>,
) -> String {
	let wifescore_floating_point_digits = match analysis
		.scoring_system_comparison_j4
		.wife3_score
		.as_percent()
		> 99.7
	{
		true => 4,
		false => 2,
	};

	let alternative_text_1;
	let alternative_text_2;
	let alternative_text_4;
	if let Some(comparison) = &analysis.scoring_system_comparison_alternative {
		// UNWRAP: if we're in this branch, info.alternative_judge is Some
		alternative_text_1 = format!(
			", {:.digits$} on {}",
			comparison.wife2_score,
			alternative_judge.unwrap().name,
			digits = wifescore_floating_point_digits,
		);
		alternative_text_2 = format!(
			", {:.digits$} on {}",
			comparison.wife3_score,
			alternative_judge.unwrap().name,
			digits = wifescore_floating_point_digits,
		);
		alternative_text_4 = format!(
			", {:.digits$} on {}",
			comparison.wife3_score_zero_mean,
			alternative_judge.unwrap().name,
			digits = wifescore_floating_point_digits,
		);
	} else {
		alternative_text_1 = "".to_owned();
		alternative_text_2 = "".to_owned();
		alternative_text_4 = "".to_owned();
	}

	let mut score_comparisons_text = String::new();

	if (analysis
		.scoring_system_comparison_j4
		.wife3_score
		.as_percent()
		- score.wifescore.as_percent())
	.abs() > 0.01
	{
		score_comparisons_text += "_Note: these calculated scores are slightly inaccurate_\n";
	}

	score_comparisons_text += &format!(
		"\
**Wife2**: {:.digits$}%{}
**Wife3**: {:.digits$}%{}
**Wife3**: {:.digits$}%{} (mean of {:.1}ms corrected)",
		analysis
			.scoring_system_comparison_j4
			.wife2_score
			.as_percent(),
		alternative_text_1,
		analysis
			.scoring_system_comparison_j4
			.wife3_score
			.as_percent(),
		alternative_text_2,
		analysis
			.scoring_system_comparison_j4
			.wife3_score_zero_mean
			.as_percent(),
		alternative_text_4,
		analysis.mean_offset * 1000.0,
		digits = wifescore_floating_point_digits,
	);

	score_comparisons_text
}

pub async fn send_score_card(ctx: Context<'_>, info: ScoreCard<'_>) -> Result<(), Error> {
	let score = ctx.data().v1.score_data(info.scorekey).await?;

	let alternative_judge_wifescore = match (info.alternative_judge, &score.replay) {
		(Some(alternative_judge), Some(replay)) => {
			etterna::rescore_from_note_hits::<etterna::Wife3, _>(
				replay.notes.iter().map(|note| note.hit),
				score.judgements.hit_mines,
				score.judgements.let_go_holds + score.judgements.missed_holds,
				alternative_judge,
			)
		}
		_ => None,
	};

	let description = write_score_card_body(&info, &score, alternative_judge_wifescore);

	let replay_analysis = do_replay_analysis(&score, info.alternative_judge).transpose()?;

	let mut embed = serenity::CreateEmbed::default();
	embed
		.color(crate::ETTERNA_COLOR)
		.author(|a| {
			a.name(&score.song.name)
				.url(format!(
					"https://etternaonline.com/song/view/{}",
					score.song.id
				))
				.icon_url(format!(
					"https://etternaonline.com/img/flags/{}.png",
					score.user.country_code.as_deref().unwrap_or("")
				))
		})
		// .thumbnail(format!("https://etternaonline.com/avatars/{}", score.user.avatar)) // takes too much space
		.description(description)
		.timestamp(score.datetime.as_str())
		.footer(|f| {
			f.text(format!("Played by {}", &score.user.username,))
				.icon_url(format!(
					"https://etternaonline.com/avatars/{}",
					score.user.avatar
				))
		});

	if let Some(analysis) = &replay_analysis {
		embed
			.attachment(analysis.replay_graph_path)
			.field(
				"Score comparisons",
				generate_score_comparisons_text(&score, analysis, info.alternative_judge),
				false,
			)
			.field(
				"Tap speeds",
				format!(
					"\
Fastest jack over a course of 20 notes: {:.2} NPS
Fastest total NPS over a course of 100 notes: {:.2} NPS",
					analysis.fastest_finger_jackspeed, analysis.fastest_nps,
				),
				false,
			)
			.field(
				"Combos",
				format!(
					"\
Longest combo: {}
Longest perfect combo: {}
Longest marvelous combo: {}
Longest 100% combo: {}
",
					analysis.longest_combo,
					analysis.longest_perf_combo,
					analysis.longest_marv_combo,
					analysis.longest_100_combo,
				),
				false,
			);

		if !analysis.fun_facts.is_empty() {
			embed.field("Fun facts", analysis.fun_facts.join("\n"), false);
		}
	}

	poise::send_reply(ctx, |f: &mut poise::CreateReply<'_>| {
		f.embed(|e| {
			*e = embed;
			e
		});
		if let Some(analysis) = &replay_analysis {
			f.attachment(analysis.replay_graph_path.into());
		}
		f
	})
	.await?;

	Ok(())
}
