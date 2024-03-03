import type { Feature, NodeMap } from './types';
import { BboxBitmap } from './bbox-bitmap';

// TODO: why can't we import this? get some weird type error.
const pointInPolygon = require('@turf/boolean-point-in-polygon').default;
const fs = require('node:fs');

export const load = (filename: string): Feature[] => {
	const lines = fs.readFileSync(filename, 'utf-8')
		.split('\n')
		.filter((x: string) => x)
		.map((x: string) => JSON.parse(x));
	//console.log(lines);
	console.log(lines.length);
	return lines;
}

export const ll = (coord: [number, number]): string => coord.join(',');

export const buildIndex = (features: Feature[]): NodeMap => {
	const map: NodeMap = {};

	const index = (properties: Feature['properties'], coordinates: [number, number], neighbour?: [number, number]) => {
		const key = ll(coordinates);

		if (!map[key])
			map[key] = {
				ll: coordinates,
				nodes: [],
				edges: [],
				inParkingLot: false
			};

		const v = map[key];

		v.nodes.push(properties);

		if (neighbour)
			v.edges.push(neighbour);

		if (isParkingLot(properties))
			v.inParkingLot = true;
	};

	const parkingLotBitmap = BboxBitmap();
	const parkingLots: Feature[] = [];

	for (const feature of features) {
		if (isParkingLot(feature.properties)) {
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

	// CONSIDER: This could be parallelized, it's ~80% of the
	// runtime.
	for (const v of Object.values(map)) {
		if (!parkingLotBitmap.test(v.ll))
			continue;

		for (const feature of parkingLots) {
			if (feature.geometry.type !== 'MultiPolygon')
				continue;

			if (isParkingLot(feature.properties) || pointInPolygon(v.ll, feature))
				v.inParkingLot = true;
		}
	}

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
		const paths = v.nodes.filter(isFootPath);
		const roads = v.nodes.filter(isRoad);
		if ((v.inParkingLot || roads.length >= 1) && paths.length) {
			rv.push(ll);
		}
	}
	return rv;
};
