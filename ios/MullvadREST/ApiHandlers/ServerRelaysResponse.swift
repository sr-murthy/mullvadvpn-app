//
//  ServerRelaysResponse.swift
//  MullvadREST
//
//  Created by pronebird on 27/07/2021.
//  Copyright © 2025 Mullvad VPN AB. All rights reserved.
//

import Foundation
import MullvadTypes
import Network

extension REST {
    public struct ServerLocation: Codable, Equatable, Sendable {
        public let country: String
        public let city: String
        public let latitude: Double
        public let longitude: Double

        public init(country: String, city: String, latitude: Double, longitude: Double) {
            self.country = country
            self.city = city
            self.latitude = latitude
            self.longitude = longitude
        }
    }

    public struct BridgeRelay: Codable, Equatable, Sendable {
        public let hostname: String
        public let active: Bool
        public let owned: Bool
        public let location: LocationIdentifier
        public let provider: String
        public let ipv4AddrIn: IPv4Address
        public let weight: UInt64
        public let includeInCountry: Bool
        public var daita: Bool?

        public func override(ipv4AddrIn: IPv4Address?) -> Self {
            BridgeRelay(
                hostname: hostname,
                active: active,
                owned: owned,
                location: location,
                provider: provider,
                ipv4AddrIn: ipv4AddrIn ?? self.ipv4AddrIn,
                weight: weight,
                includeInCountry: includeInCountry
            )
        }
    }

    // swiftlint:disable nesting
    public struct ServerRelay: Codable, Equatable, Sendable {
        public struct Features: Codable, Equatable, Sendable {
            public struct DAITA: Codable, Equatable, Sendable {
                // this structure intentionally left blank
            }

            public struct QUIC: Codable, Equatable, Sendable {
                public let addrIn: [String]
                public let domain: String
                public let token: String
            }

            public let daita: DAITA?
            public let quic: QUIC?
        }

        public let hostname: String
        public let active: Bool
        public let owned: Bool
        public let location: LocationIdentifier
        public let provider: String
        public let weight: UInt64
        public let ipv4AddrIn: IPv4Address
        public let ipv6AddrIn: IPv6Address
        public let publicKey: Data
        public let includeInCountry: Bool
        public let daita: Bool?
        public let shadowsocksExtraAddrIn: [String]?
        public let features: Features?

        public func override(ipv4AddrIn: IPv4Address?, ipv6AddrIn: IPv6Address?) -> Self {
            ServerRelay(
                hostname: hostname,
                active: active,
                owned: owned,
                location: location,
                provider: provider,
                weight: weight,
                ipv4AddrIn: ipv4AddrIn ?? self.ipv4AddrIn,
                ipv6AddrIn: ipv6AddrIn ?? self.ipv6AddrIn,
                publicKey: publicKey,
                includeInCountry: includeInCountry,
                daita: daita,
                shadowsocksExtraAddrIn: shadowsocksExtraAddrIn?.filter { address in
                    return switch address {
                    case let ip where IPv4Address(ip) != nil:
                        ipv4AddrIn == nil
                    case let ip where IPv6Address(ip) != nil:
                        ipv6AddrIn == nil
                    default:
                        true
                    }
                },
                features: features
            )
        }

        // this is for the legacy DAITA flag, which will be deprecated in favour of a DAITA structure under Features
        public func override(daita: Bool) -> Self {
            ServerRelay(
                hostname: hostname,
                active: active,
                owned: owned,
                location: location,
                provider: provider,
                weight: weight,
                ipv4AddrIn: ipv4AddrIn,
                ipv6AddrIn: ipv6AddrIn,
                publicKey: publicKey,
                includeInCountry: includeInCountry,
                daita: daita,
                shadowsocksExtraAddrIn: shadowsocksExtraAddrIn,
                features: features
            )
        }

        public func override(features: ServerRelay.Features) -> Self {
            ServerRelay(
                hostname: hostname,
                active: active,
                owned: owned,
                location: location,
                provider: provider,
                weight: weight,
                ipv4AddrIn: ipv4AddrIn,
                ipv6AddrIn: ipv6AddrIn,
                publicKey: publicKey,
                includeInCountry: includeInCountry,
                daita: daita,
                shadowsocksExtraAddrIn: shadowsocksExtraAddrIn,
                features: features
            )
        }

        public var hasDaita: Bool {
            (features?.daita != nil) || daita == true
        }
    }

    // swiftlint:enable nesting

    public struct ServerWireguardTunnels: Codable, Equatable, Sendable {
        public let ipv4Gateway: IPv4Address
        public let ipv6Gateway: IPv6Address
        public let portRanges: [[UInt16]]
        public let relays: [ServerRelay]
        public let shadowsocksPortRanges: [[UInt16]]

        public init(
            ipv4Gateway: IPv4Address,
            ipv6Gateway: IPv6Address,
            portRanges: [[UInt16]],
            relays: [ServerRelay],
            shadowsocksPortRanges: [[UInt16]]
        ) {
            self.ipv4Gateway = ipv4Gateway
            self.ipv6Gateway = ipv6Gateway
            self.portRanges = portRanges
            self.relays = relays
            self.shadowsocksPortRanges = shadowsocksPortRanges
        }
    }

    public struct ServerShadowsocks: Codable, Equatable, Sendable {
        public let `protocol`: String
        public let port: UInt16
        public let cipher: String
        public let password: String
    }

    public struct ServerBridges: Codable, Equatable, Sendable {
        public let shadowsocks: [ServerShadowsocks]
        public let relays: [BridgeRelay]
    }

    public struct ServerRelaysResponse: Codable, Equatable, Sendable {
        public let locations: [String: ServerLocation]
        public let wireguard: ServerWireguardTunnels
        public let bridge: ServerBridges

        public init(
            locations: [String: ServerLocation],
            wireguard: ServerWireguardTunnels,
            bridge: ServerBridges
        ) {
            self.locations = locations
            self.wireguard = wireguard
            self.bridge = bridge
        }
    }
}
