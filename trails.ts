import type { NodeMap, Trail } from './types';
import { ll2str } from './lib';

export const getTrails = (map: NodeMap, trailhead: string) => {
	const node = map[trailhead];
	if (!node)
		throw new Error(`trailhead ${trailhead} is unknown`);
	const completeTrails: Trail[] = [];

	// To generate all of the paths:
	// 1. Maintain a list of candidate paths.
	// 2. The initial list is [trailhead].
	// 3. While we have discovered new knowledge:
	//   A. For each candidate path, find reachable next nodes.
	//   B. Reject the next node in some cases:
	//      i.  It's a node we have already visited 2 times
	//      ii. There is a choice of nodes, and we have not visited the other node already
	//   C. Check if any of the paths is a complete path.

	let candidates: Trail[] = [];
	candidates.push({
		coords: [node.ll]
	});
	let didWork = true;
	let nodes = 0;
	while (didWork) {
		nodes++;
		didWork = false;

		const oldCandidates = candidates;
		candidates = [];

		for (const candidate of oldCandidates) {
			const ll = candidate.coords.at(-1)!;
			const node = map[ll2str(ll)];
			if (!node)
				throw new Error(`panic: map lacked entry for ${ll2str(ll)}`);

			const edges = node.edges;

			const counts: { [ll: string]: number } = {};
			for (const ll of candidate.coords) {
				const key = ll2str(ll);
				counts[key] = (counts[key] || 0) + 1;
			}
			const unvisitedEdges = edges.filter(edge => counts[ll2str(edge)] === undefined);

			// If the choice is backtracking or visiting a new node, only permit
			// visiting the new node.
			let candidateEdges: [number, number][] = unvisitedEdges.length > 0 ? unvisitedEdges : edges;

			// Remove any node we have visited twice -- in a lasso or out-and-back, we'll
			// traverse the same node twice, but three times is just wasteful backtracking.
			candidateEdges = candidateEdges.filter(edge => !(counts[ll2str(edge)] >= 2));

			// For each candidate edge, construct a new path.
			for (const nextCoord of candidateEdges) {
				didWork = true;
				const newTrail: Trail = {
					...candidate,
					coords: [
						...candidate.coords,
						nextCoord
					],
				};

				// Is nextCoord:
				// - the start?
				// - otherwise a terminus?
				//
				// If yes, emit a complete trail.
				// If no, put it back on the pile.
				if (ll2str(nextCoord) == ll2str(candidate.coords[0])) {
					// We have a loop or out-and-back.
					completeTrails.push(newTrail);
					//console.log(`found trail: len=${newTrail.coords.length}`);
				} else {
					// TODO: support one-way trails, e.g. Tonquin Valley Trail, where
					//       the start and the end nodes are not the same.

					// We're not done, keep lookin'.
					candidates.push(newTrail);
				}
			}
		}
	}

	// At this point, we have (hopefully!) generated multiple candidate trails.
	//
	// Many of the trails may be overlapping:
	// - clockwise loop
	// - counterclockwise loop
	//
	// We don't handle that here, that's for groupAndFilter to fix.
	return completeTrails;
}

const uniqueNodes = (a: Trail): { [ll: string]: number } => {
	const rv: { [ll: string]: number } = {};
	for (const ll of a.coords) {
		const key = ll2str(ll);
		rv[key] = (rv[key] || 0) + 1;
	}

	return rv;
};

const mostUniqueNodes = (a: Trail, b: Trail): number => {
	const uniqueA = uniqueNodes(a);
	const uniqueB = uniqueNodes(b);
	const sizeA = Object.keys(uniqueA).length;
	const sizeB = Object.keys(uniqueB).length;

	if (sizeA === sizeB) {
		return b.coords.length - a.coords.length;
	}

	return sizeB - sizeA;
};

const shortestMostUniqueNodes = (a: Trail, b: Trail): number => {
	const uniqueA = uniqueNodes(a);
	const uniqueB = uniqueNodes(b);
	const sizeA = Object.keys(uniqueA).length;
	const sizeB = Object.keys(uniqueB).length;

	if (sizeA === sizeB) {
		return a.coords.length - b.coords.length;
	}

	return sizeB - sizeA;
};


const trailContainsOtherTrail = (haystack: Trail, needle: Trail): boolean => {
	const haystackMap: { [ll: string]: true } = {};
	for (const ll of haystack.coords)
		haystackMap[ll2str(ll)] = true;

	for (const ll of needle.coords)
		if (!haystackMap[ll2str(ll)])
			return false;

	return true;
};

export const groupAndFilter = (trails: Trail[]) => {
	// `trails` is a raw list of generated trails. They may have multiple very similar trails,
	// e.g. clockwise and counterclockwise loops of the same trail.
	//
	// We now want to group them by similarity (e.g. how many of the same edges do they traverse).
	//
	// Then, for each group, rank by "quality" (e.g. covering the same edge multiple times is worse)
	// and take the best.

	// Step 1: group the trails.
	// I'm not sure what the best way to do this is. For now, two trails A and B are grouped
	// if either trail is a subset of the other.
	const groups: Trail[][] = [];

	// Sort the input trails such that biggest trails (by # of unique nodes) appear first.
	trails.sort(mostUniqueNodes);
	for (const trail of trails) {
		// Is there an existing group we can glom on to?
		let foundGroup = false;
		for (const group of groups) {
			// We only need to check the first member of the group, by definition the other members are
			// subsets of it.
			if (trailContainsOtherTrail(group[0], trail)) {
				group.push(trail);
				foundGroup = true;
				break;
			}
		}

		if (!foundGroup) {
			groups.push([trail]);
		}
	}

	// Step 2: for each group, take the "best" trail. The best trail is the trail that
	// covers the most unique edges, with ties broken by the shortest trail (i.e.
	// less backtracking)
	const rv: Trail[] = [];
	for (const group of groups) {
		group.sort(shortestMostUniqueNodes);
		rv.push(group[0]);
	}
	return rv;
}
