import type { Feature, NodeMap, Tile } from './types';
import { BboxBitmap } from './bbox-bitmap';
import { highestCommonTile, highestCommonTileForGeometry } from './coords';

// TODO: why can't we import this? get some weird type error.
const pointInPolygon = require('@turf/boolean-point-in-polygon').default;
const fs = require('node:fs');

export const load = (filename: string): Feature[] => {
	const lines = fs.readFileSync(filename, 'utf-8')
		.split('\n')
		.filter((x: string) => x)
		.map((x: string) => JSON.parse(x));
	//console.log(lines);
//	console.log(lines.length);
	return lines;
}

export const ll2str = (coord: [number, number]): string => coord.join(',');

export const tile2str = (tile: Tile): string => `z${tile.z}/${tile.x}/${tile.y}`;

export const buildIndex = (features: Feature[]): NodeMap => {
	const map: NodeMap = {};

	const parkingLotTiles: { [key: string]: Feature[] } = {};

	const index = (properties: Feature['properties'], coordinates: [number, number], neighbour?: [number, number]) => {
		const key = ll2str(coordinates);

		let v = map[key];
		if (!v) {
			v = map[key] = {
				ll: coordinates,
				nodes: [],
				edges: [],
				inParkingLot: false
			};
		}

		// Ways often contain the same node twice; don't add it twice.
		const found = !!v.nodes.find(n => n === properties);

		if (!found) {
				v.nodes.push(properties);
		}

		if (neighbour) {
			v.edges.push(neighbour);

			// Draw an implicit edge back from neighbour.
			// I believe this is valid.
			//
			// Although ways in OSM are directed graphs, I don't think the order
			// of the way implies onewayness. To do that, you must use the
			// https://wiki.openstreetmap.org/wiki/Key:oneway key.
			const neighbourKey = ll2str(neighbour);
			let neighbourV = map[neighbourKey];
			if (!neighbourV) {
				neighbourV = map[neighbourKey] = {
					ll: neighbour,
					nodes: [],
					edges: [],
					inParkingLot: false
				};
			}

			neighbourV.edges.push(coordinates);

			const neighbourFound = !!neighbourV.nodes.find(n => n === properties);
			if (!neighbourFound)
				neighbourV.nodes.push(properties);
		}

		if (isParkingLot(properties))
			v.inParkingLot = true;
	};

	const parkingLotBitmap = BboxBitmap();
	const parkingLots: Feature[] = [];

	let start = Date.now();
	for (const feature of features) {
		if (isParkingLot(feature.properties)) {
			const tile = highestCommonTileForGeometry(feature.geometry);
			const tileKey = tile2str(tile);
			parkingLotTiles[tileKey] = parkingLotTiles[tileKey] || [];
			parkingLotTiles[tileKey].push(feature);
			parkingLots.push(feature);

			if (feature.geometry.type === 'MultiPolygon')
				parkingLotBitmap.add(feature);
		}

		if (feature.geometry.type === 'Point') {
			index(feature.properties, feature.geometry.coordinates);
		} else if (feature.geometry.type === 'LineString') {
			const coords = feature.geometry.coordinates;
			for (let i = 0; i < coords.length; i++)
				index(feature.properties, coords[i], coords[i + 1]);
		} else if (feature.geometry.type === 'MultiLineString') {
		} else if (feature.geometry.type === 'MultiPolygon') {
			// Just index the outer rings, e.g. for parking lots
			for (const poly of feature.geometry.coordinates) {
				const coords = poly[0];
				// The last coord is the same as the first coord, so skip it.
				for (let i = 0; i < coords.length - 1; i++)
					index(feature.properties, coords[i], coords[i + 1]);
			}
		}
	}
	//console.log(`pass 1 took ${Date.now() - start} ms`);
	start = Date.now();

	// CONSIDER: This could be parallelized, it's ~80% of the
	// runtime.
	for (const v of Object.values(map)) {
		if (!parkingLotBitmap.test(v.ll))
			continue;

		let needle = highestCommonTile([v.ll]);

		const overlappingParkingLots = [];
		while(needle.z > 0) {
			const key = tile2str(needle);

			const toAdd = parkingLotTiles[key];
			if (toAdd)
				overlappingParkingLots.push(...toAdd);

			needle.z--;
			needle.x = Math.floor(needle.x / 2);
			needle.y = Math.floor(needle.y / 2);
		}

		for (const feature of overlappingParkingLots) {
			if (feature.geometry.type !== 'MultiPolygon')
				continue;

			if (pointInPolygon(v.ll, feature))
				v.inParkingLot = true;
		}
	}
	//console.log(`pass 2 took ${Date.now() - start} ms`);

	return map;
}

export const isParkingLot = (node: Feature['properties']): boolean => {
	return node['amenity'] === 'parking';
}

export const isFootPath = (node: Feature['properties']): boolean => {
	return node['highway'] === 'footway' || // Mizzy Lake Trail
		node['highway'] === 'path' // Bat Lake Trail;
}

export const isRoad = (node: Feature['properties']): boolean => {
	return node['highway'] === 'motorway' ||
		node['highway'] === 'trunk' ||
		node['highway'] === 'primary' ||
		node['highway'] === 'secondary' ||
		node['highway'] === 'tertiary' ||
		node['highway'] === 'unclassified' ||
		node['highway'] === 'residential' ||
		node['highway'] === 'service';

	// Consider: service, track, road
}

export const getTrailheads = (map: NodeMap): string[] => {
	const rv: string[] = [];

	for (const [ll, v] of Object.entries(map)) {
		const paths = v.nodes.find(isFootPath);
		if (!paths)
			continue;

		const roads = v.nodes.find(isRoad);
		if (v.inParkingLot || roads) {
			rv.push(ll);
		}
	}
	return rv;
};

export interface Trail {
	coords: [number, number][];
}

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
			/*
			console.log(node);
			if (nodes > 2)
				return;
				*/

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
				} else {
					// TODO: support one-way trails, e.g. Tonquin Valley Trail, where
					//       the start and the end nodes are not the same.

					// We're not done, keep lookin'.
					candidates.push(newTrail);
				}
			}
		}
	}

	// Q: why is this 1 trail and not 2?
	console.log(completeTrails.length);
	return completeTrails;
}
